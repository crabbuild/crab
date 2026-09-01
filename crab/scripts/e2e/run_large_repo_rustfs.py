#!/usr/bin/env python3
"""Qualify Crab with a real large Git repository against RustFS.

The default workload uses the existing read-only Kubernetes checkout below
``~/Workspace/Github/kubernetes/kubernetes``. It pushes ``HEAD~1000``
as the seed, replays the final 1,000 first-parent commits one at a time, and
measures full, filtered, shallow, and incremental reads. All generated local
state lives below ``~/Workspace/CrabBuild/crabbuild-qualification``.

The script never cleans or modifies the source checkout. Remote cleanup, when
requested, is restricted to the unique repository prefix created by this run.
"""

from __future__ import annotations

import argparse
import hashlib
import heapq
import json
import math
import os
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterable

SCHEMA = "crab.large-repository-rustfs"
VERSION = "1.3"
WORKSPACE_ROOT = Path.home() / "Workspace"
DEFAULT_SOURCE = WORKSPACE_ROOT / "Github" / "kubernetes" / "kubernetes"
DEFAULT_ROOT = WORKSPACE_ROOT / "CrabBuild" / "crabbuild-qualification"
DEFAULT_BUCKET = "crab"
DEFAULT_ENDPOINT = "http://127.0.0.1:9000"
DEFAULT_REPLAY_COUNT = 1_000
DEFAULT_SAMPLE_SIZE = 1_000
# A repack can make catalog, visibility, graph, and shallow proofs stale in
# sequence. Bounded repacks add one maintenance wave per selected pack batch,
# so four samples per observed pack leave room for each catalog/visibility step
# while the guard remains finite.
BASE_GENERATION_OWNER_PASSES = 16
GENERATION_OWNER_PASSES_PER_PACK = 4
REMOTE_ROOT = "e2e-large-repository"
RUN_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
OID_RE = re.compile(r"^[0-9a-f]{40}$")
SECRET_KEYS = {
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "CRAB_CACHE_PSK",
    "CRAB_CACHE_TOKEN",
}
CACHE_SERVICE_URL_ENV = "CRAB_CACHE_SERVICE_URL"
CACHE_SERVICE_PSK_ENV = "CRAB_CACHE_PSK"
CACHE_SERVICE_TOKEN_ENV = "CRAB_CACHE_TOKEN"
CACHE_SERVICE_CAPABILITIES_SCHEMA = "crab-cache-service.capabilities.v1"
CACHE_ROUTE_SCHEMA = "crab-cache-service.routes.v3"
SCRIPT_DIR = Path(__file__).resolve().parent
CRAB_DIR = SCRIPT_DIR.parents[1]
REPO_ROOT = SCRIPT_DIR.parents[2]
START_RUSTFS = CRAB_DIR / "scripts" / "start-rustfs.sh"
QUALIFICATION_DEBUG_LOG = (
    "crab=debug,crab_coordination=debug,crab_remote_git=info,"
    "crab_read::upload_pack=debug,"
    "crab_metadata::git_object_locator::reader=debug"
)


class QualificationError(RuntimeError):
    """Raised when qualification cannot produce trustworthy evidence."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def default_run_id() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def safe_run_id(value: str) -> str:
    if not RUN_ID_RE.fullmatch(value):
        raise QualificationError(
            "run id may contain only letters, numbers, dot, underscore, or dash"
        )
    return value


def slug(value: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9_.-]+", "-", value.lower()).strip("-")
    return cleaned or "command"


def percentile(values: list[int], percent: float) -> int:
    if not values:
        return 0
    ordered = sorted(values)
    index = max(0, math.ceil(percent * len(ordered)) - 1)
    return ordered[index]


def redact_text(value: str, secrets: Iterable[str]) -> str:
    result = value
    for secret in sorted({secret for secret in secrets if secret}, key=len, reverse=True):
        result = result.replace(secret, "<redacted>")
    return result


def resolve_executable(value: str, label: str) -> Path:
    candidate = Path(value)
    if not candidate.is_absolute():
        located = shutil.which(value)
        if located:
            candidate = Path(located)
    candidate = candidate.resolve()
    if not candidate.is_file() or not os.access(candidate, os.X_OK):
        raise QualificationError(f"{label} is not executable: {candidate}")
    return candidate


def snapshot_executable(source: Path, destination: Path, label: str) -> Path:
    """Copy an executable into a run-owned path and return the verified copy."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f".{destination.name}.{os.getpid()}.tmp")
    try:
        shutil.copy2(source, temporary)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return resolve_executable(str(destination), label)


class LargeRepositoryQualification:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.run_id = safe_run_id(args.run_id or default_run_id())
        self.run_root = args.root.resolve() / self.run_id
        self.logs = self.run_root / "logs"
        self.artifacts = self.run_root / "artifacts"
        self.temp_root = self.run_root / "tmp"
        self.cache_root = self.run_root / "cache"
        self.bin_root = self.run_root / "bin"
        self.replay_repo = self.run_root / "replay"
        self.incremental_clone = self.run_root / "incremental-clone"
        self.clone_root = self.run_root / "clones"
        self.fetch_root = self.run_root / "fetch-clients"
        self.team_root = self.run_root / "team-clients"
        self.source = args.source.resolve()
        self.crab_source_bin = resolve_executable(args.crab_bin, "Crab binary")
        self.crab_bin = self.bin_root / "crab"
        self.git_bin = resolve_executable(args.git_bin, "Git binary")
        self.aws_bin = resolve_executable(args.aws_bin, "AWS CLI")
        self.cache_service_url = os.environ.get(CACHE_SERVICE_URL_ENV, "").strip()
        self.remote_prefix = f"{REMOTE_ROOT}/{self.run_id}"
        self.remote_url = f"crab://{args.bucket}/{self.remote_prefix}"
        self.command_index = 0
        self.report_lock = threading.RLock()
        self.env = self.build_env()
        self.report: dict[str, Any] = {
            "schema": SCHEMA,
            "version": VERSION,
            "profile": "full" if args.replay_count >= 1_000 else "smoke",
            "run_id": self.run_id,
            "status": "running",
            "valid_for_comparison": True,
            "comparison_invalid_reason": None,
            "started_at": utc_now(),
            "finished_at": None,
            "root": str(self.run_root),
            "source": {},
            "remote": {
                "url": self.remote_url,
                "bucket": args.bucket,
                "prefix": self.remote_prefix,
                "endpoint_url": args.endpoint_url,
            },
            "cache_service": {
                "configured": bool(self.cache_service_url),
                "required": args.require_cache_service,
                "url": self.cache_service_url or None,
                "health_status": None,
                "capabilities_status": None,
                "capabilities_schema": None,
                "route_schema": None,
                "stats": None,
            },
            "provenance": {},
            "commands": [],
            "checks": [],
            "stages": {},
            "pushes": [],
            "team_load": {},
            "store_snapshots": [],
            "correctness": {},
            "metrics": {},
            "artifacts": {},
            "cleanup": {
                "remote_requested": args.cleanup_remote,
                "remote_completed": False,
                "local_worktrees_retained": args.retain_worktrees,
                "local_worktrees_removed": False,
            },
            "error": None,
        }

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
                "GIT_AUTHOR_NAME": "Crab large repository qualification",
                "GIT_AUTHOR_EMAIL": "large-repository@example.invalid",
                "GIT_COMMITTER_NAME": "Crab large repository qualification",
                "GIT_COMMITTER_EMAIL": "large-repository@example.invalid",
                "CRAB_CACHE_DIR": str(self.cache_root),
                "CRAB_LOG": "crab=info,crab_remote_git=info",
                "TMPDIR": str(self.temp_root),
                "TMP": str(self.temp_root),
                "TEMP": str(self.temp_root),
            }
        )
        if self.args.endpoint_url:
            env["AWS_ENDPOINT_URL"] = self.args.endpoint_url
            env["AWS_ENDPOINT_URL_S3"] = self.args.endpoint_url
        if self.args.session_token:
            env["AWS_SESSION_TOKEN"] = self.args.session_token
        else:
            env.pop("AWS_SESSION_TOKEN", None)
        env["PATH"] = str(self.bin_root) + os.pathsep + env.get("PATH", "")
        return env

    def secret_values(self) -> tuple[str, ...]:
        return tuple(
            value
            for value in (
                self.args.access_key,
                self.args.secret_key,
                self.args.session_token,
                self.env.get(CACHE_SERVICE_PSK_ENV),
                self.env.get(CACHE_SERVICE_TOKEN_ENV),
            )
            if value and value != "crab"
        )

    def write_report(self) -> None:
        with self.report_lock:
            self.artifacts.mkdir(parents=True, exist_ok=True)
            report_path = self.artifacts / "report.json"
            self.report["artifacts"]["report"] = str(report_path)
            body = json.dumps(self.report, indent=2, sort_keys=True) + "\n"
            temporary = report_path.with_suffix(".json.tmp")
            temporary.write_text(body, encoding="utf-8")
            temporary.replace(report_path)

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        with self.report_lock:
            self.report["checks"].append(
                {
                    "name": name,
                    "ok": ok,
                    "detail": detail or {},
                    "checked_at": utc_now(),
                }
            )
        self.write_report()
        if not ok:
            raise QualificationError(f"check failed: {name}")

    def install_helper_alias(self) -> None:
        self.bin_root.mkdir(parents=True, exist_ok=True)
        alias = self.bin_root / "git-remote-crab"
        if alias.exists() or alias.is_symlink():
            alias.unlink()
        try:
            alias.symlink_to(self.crab_bin)
        except OSError:
            shutil.copy2(self.crab_bin, alias)

    def process_tree_resources(self, root_pid: int) -> tuple[int, int, int]:
        try:
            output = subprocess.run(
                ["ps", "-axo", "pid=,ppid=,rss=,utime=,stime="],
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                check=False,
            ).stdout
        except OSError:
            return 0, 0, 0
        processes: dict[int, tuple[int, int, int, int]] = {}
        for line in output.splitlines():
            fields = line.split()
            if len(fields) != 5:
                continue
            try:
                pid, parent, rss_kib = (int(field) for field in fields[:3])
                user_ms = self.cpu_time_ms(fields[3])
                system_ms = self.cpu_time_ms(fields[4])
            except ValueError:
                continue
            processes[pid] = (parent, rss_kib * 1024, user_ms, system_ms)
        children: dict[int, list[int]] = {}
        for pid, (parent, _rss, _user, _system) in processes.items():
            children.setdefault(parent, []).append(pid)
        pending = [root_pid]
        tree: set[int] = set()
        while pending:
            pid = pending.pop()
            if pid in tree:
                continue
            tree.add(pid)
            pending.extend(children.get(pid, []))
        return (
            sum(processes[pid][1] for pid in tree if pid in processes),
            sum(processes[pid][2] for pid in tree if pid in processes),
            sum(processes[pid][3] for pid in tree if pid in processes),
        )

    @staticmethod
    def cpu_time_ms(value: str) -> int:
        day_parts = value.split("-", 1)
        days = int(day_parts[0]) if len(day_parts) == 2 else 0
        clock = day_parts[-1].split(":")
        if len(clock) == 3:
            hours, minutes, seconds = int(clock[0]), int(clock[1]), float(clock[2])
        elif len(clock) == 2:
            hours, minutes, seconds = 0, int(clock[0]), float(clock[1])
        else:
            raise ValueError(f"unsupported process CPU time: {value}")
        return int((((days * 24 + hours) * 60 + minutes) * 60 + seconds) * 1_000)

    def terminate_process(self, process: subprocess.Popen[bytes]) -> None:
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

    def telemetry_from_log(self, path: Path) -> dict[str, int]:
        telemetry: dict[str, int] = {
            "storage_requests": 0,
            "storage_bytes": 0,
            "range_get": 0,
            "range_get_coalesced": 0,
            "locator_lookup": 0,
            "cache_hits": 0,
            "cache_misses": 0,
            "logical_objects": 0,
            "inflated_bytes": 0,
            "response_bytes": 0,
            "operation_duration_ms": 0,
            "visibility_duration_ms": 0,
            "upload_pack_duration_ms": 0,
            "visibility_plan_ms": 0,
            "pack_generation_ms": 0,
            "source_download_ms": 0,
            "source_inventory_check_ms": 0,
            "locator_scan": 0,
            "locator_full_scan": 0,
            "locator_exact_fallback": 0,
            "locator_ordinal_scan": 0,
            "locator_ordinal_metadata": 0,
            "locator_ordinal_metadata_scan": 0,
        }
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            fields = event.get("fields")
            if not isinstance(fields, dict):
                continue
            lookup_mode = {
                "scan": "locator_scan",
                "full_scan": "locator_full_scan",
                "exact_fallback": "locator_exact_fallback",
                "ordinal_scan": "locator_ordinal_scan",
                "ordinal_metadata": "locator_ordinal_metadata",
                "ordinal_metadata_scan": "locator_ordinal_metadata_scan",
            }.get(str(fields.get("locator_lookup_mode", "")))
            if lookup_mode is not None:
                telemetry[lookup_mode] += 1
            request = fields.get("storage_request")
            if request:
                request = str(request)
                if request != "range_get_coalesced":
                    telemetry["storage_requests"] += 1
                telemetry["storage_bytes"] += int(fields.get("storage_bytes", 0))
                if request in telemetry:
                    telemetry[request] += 1
            cache_event = str(fields.get("cache_event", "")).casefold()
            if cache_event == "hit":
                telemetry["cache_hits"] += 1
            elif cache_event == "miss":
                telemetry["cache_misses"] += 1
            if fields.get("telemetry_event") == "operation_summary":
                telemetry["storage_requests"] += int(fields.get("storage_requests", 0))
                telemetry["storage_bytes"] += int(fields.get("fetched_bytes", 0))
                telemetry["logical_objects"] += int(fields.get("logical_objects", 0))
                telemetry["inflated_bytes"] += int(fields.get("inflated_bytes", 0))
                telemetry["response_bytes"] += int(fields.get("response_bytes", 0))
                duration = int(fields.get("duration_ms", 0))
                telemetry["operation_duration_ms"] += duration
                operation = fields.get("operation")
                if operation == "visibility":
                    telemetry["visibility_duration_ms"] += duration
                elif operation == "upload_pack":
                    telemetry["upload_pack_duration_ms"] += duration
            if fields.get("telemetry_event") == "visibility_plan":
                telemetry["visibility_plan_ms"] += int(
                    fields.get("visibility_plan_ms", 0)
                )
            if fields.get("telemetry_event") == "pack_generation":
                telemetry["pack_generation_ms"] += int(
                    fields.get("pack_generation_ms", 0)
                )
                telemetry["source_download_ms"] += int(
                    fields.get("source_download_ms", 0)
                )
                telemetry["source_inventory_check_ms"] += int(
                    fields.get("source_inventory_check_ms", 0)
                )
        return telemetry

    def run_cmd(
        self,
        name: str,
        args: list[str],
        cwd: Path,
        *,
        check: bool = True,
        timeout: int | None = None,
        input_data: bytes | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> dict[str, Any]:
        with self.report_lock:
            self.command_index += 1
            command_index = self.command_index
        base = f"{command_index:05d}-{slug(name)}"
        self.logs.mkdir(parents=True, exist_ok=True)
        stdout_path = self.logs / f"{base}.stdout.log"
        stderr_path = self.logs / f"{base}.stderr.log"
        env = self.env.copy()
        if extra_env:
            env.update(extra_env)
        started = time.monotonic()
        rss_peak = 0
        user_cpu_ms = 0
        system_cpu_ms = 0
        timed_out = False
        process: subprocess.Popen[bytes] | None = None
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            try:
                process = subprocess.Popen(
                    args,
                    cwd=cwd,
                    env=env,
                    stdin=subprocess.PIPE if input_data is not None else subprocess.DEVNULL,
                    stdout=stdout,
                    stderr=stderr,
                    start_new_session=os.name != "nt",
                    creationflags=(
                        subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
                    ),
                )
                if process.stdin is not None:
                    try:
                        process.stdin.write(input_data)
                        process.stdin.close()
                    except BrokenPipeError:
                        pass
                command_timeout = timeout or self.args.timeout
                while process.poll() is None:
                    rss, user, system = self.process_tree_resources(process.pid)
                    rss_peak = max(rss_peak, rss)
                    user_cpu_ms = max(user_cpu_ms, user)
                    system_cpu_ms = max(system_cpu_ms, system)
                    if time.monotonic() - started >= command_timeout:
                        self.terminate_process(process)
                        timed_out = True
                        break
                    time.sleep(self.args.sample_interval)
                exit_code = process.wait()
            except BaseException:
                if process is not None and process.poll() is None:
                    self.terminate_process(process)
                    process.wait()
                raise
        if timed_out:
            exit_code = 124
            with stderr_path.open("a", encoding="utf-8") as stderr:
                stderr.write(f"command timed out after {timeout or self.args.timeout} seconds\n")
        for path in (stdout_path, stderr_path):
            redacted = redact_text(
                path.read_text(encoding="utf-8", errors="replace"),
                self.secret_values(),
            )
            path.write_text(redacted, encoding="utf-8")
        record = {
            "name": name,
            "args": args,
            "cwd": str(cwd),
            "required_success": check,
            "exit_code": exit_code,
            "duration_ms": int((time.monotonic() - started) * 1_000),
            "resources": {
                "user_cpu_ms": user_cpu_ms,
                "system_cpu_ms": system_cpu_ms,
                "children_max_rss": rss_peak,
                "children_max_rss_unit": "bytes",
            },
            "telemetry": self.telemetry_from_log(stderr_path),
            "stdout_log": str(stdout_path),
            "stderr_log": str(stderr_path),
        }
        with self.report_lock:
            self.report["commands"].append(record)
        self.write_report()
        if check and exit_code != 0:
            raise QualificationError(
                f"{name} failed with exit {exit_code}; stderr={stderr_path}"
            )
        return record

    def stdout(self, record: dict[str, Any]) -> str:
        return Path(record["stdout_log"]).read_text(encoding="utf-8", errors="replace")

    def stderr(self, record: dict[str, Any]) -> str:
        return Path(record["stderr_log"]).read_text(encoding="utf-8", errors="replace")

    def run_git(
        self,
        cwd: Path,
        args: list[str],
        name: str,
        **kwargs: Any,
    ) -> dict[str, Any]:
        return self.run_cmd(name, [str(self.git_bin), *args], cwd, **kwargs)

    def run_crab(
        self,
        cwd: Path,
        args: list[str],
        name: str,
        **kwargs: Any,
    ) -> dict[str, Any]:
        return self.run_cmd(name, [str(self.crab_bin), *args], cwd, **kwargs)

    def git_value(self, cwd: Path, args: list[str], name: str) -> str:
        record = self.run_git(cwd, args, name)
        value = self.stdout(record).strip()
        if not value:
            raise QualificationError(f"{name} returned an empty value")
        return value

    def setup(self) -> None:
        if self.run_root.exists():
            raise QualificationError(f"run root already exists: {self.run_root}")
        self.logs.mkdir(parents=True)
        self.artifacts.mkdir()
        self.temp_root.mkdir()
        self.cache_root.mkdir()
        self.clone_root.mkdir()
        if self.args.team_load:
            self.fetch_root.mkdir()
            self.team_root.mkdir()
        # Long qualifications must not observe an unrelated `make install`
        # replacing the shared binary midway through the command sequence.
        self.crab_bin = snapshot_executable(
            self.crab_source_bin,
            self.crab_bin,
            "run-local Crab binary",
        )
        self.install_helper_alias()
        self.write_report()

    def preflight(self) -> tuple[str, str, list[str]]:
        self.check(
            "workspace-volume",
            self.args.root.is_absolute() and self.args.root.is_relative_to(WORKSPACE_ROOT),
            {"root": str(self.args.root)},
        )
        self.check(
            "source-is-git-repository",
            self.source.is_dir(),
            {"source": str(self.source)},
        )
        source_head = self.git_value(self.source, ["rev-parse", "HEAD"], "source HEAD")
        if not OID_RE.fullmatch(source_head):
            raise QualificationError("source HEAD is not a SHA-1 object ID")
        # Read status without requiring a non-empty value. The checkout is never modified.
        status_record = self.run_git(
            self.source,
            ["status", "--porcelain=v1", "--untracked-files=all"],
            "capture source status",
        )
        source_status = self.stdout(status_record)
        base = self.git_value(
            self.source,
            ["rev-parse", f"{source_head}~{self.args.replay_count}"],
            "replay base",
        )
        commits_record = self.run_git(
            self.source,
            [
                "rev-list",
                "--first-parent",
                "--reverse",
                f"{base}..{source_head}",
            ],
            "first-parent replay commits",
        )
        commits = [line for line in self.stdout(commits_record).splitlines() if line]
        self.check(
            "replay-commit-count",
            len(commits) == self.args.replay_count
            and all(OID_RE.fullmatch(commit) for commit in commits),
            {"expected": self.args.replay_count, "actual": len(commits)},
        )
        free = shutil.disk_usage(self.args.root).free
        self.check(
            "workspace-free-space",
            free >= self.args.minimum_free_bytes,
            {"free_bytes": free, "required_bytes": self.args.minimum_free_bytes},
        )
        self.report["source"] = {
            "path": str(self.source),
            "revision": source_head,
            "base_revision": base,
            "replay_count": len(commits),
            "status_sha256": hashlib.sha256(source_status.encode()).hexdigest(),
        }
        aws_version = self.run_cmd(
            "AWS CLI version",
            [str(self.aws_bin), "--version"],
            self.run_root,
        )
        crab_build_record = self.run_crab(
            self.run_root,
            ["version", "--json"],
            "Crab build provenance",
        )
        crab_build_envelope = json.loads(self.stdout(crab_build_record))
        crab_build = crab_build_envelope.get("data")
        if not isinstance(crab_build, dict):
            raise QualificationError("crab version --json is missing build provenance")
        self.report["provenance"] = {
            "git": self.git_value(self.run_root, ["--version"], "Git version"),
            "crab": self.stdout(
                self.run_crab(self.run_root, ["--version"], "Crab version")
            ).strip(),
            "crab_build": crab_build,
            "crab_binary_sha256": hashlib.sha256(self.crab_bin.read_bytes()).hexdigest(),
            "aws": (self.stdout(aws_version) or self.stderr(aws_version)).strip(),
            "python": platform.python_version(),
            "platform": platform.platform(),
            "host": platform.node(),
            "cpu_count": os.cpu_count() or 0,
            "crab_source_revision": self.git_value(
                REPO_ROOT, ["rev-parse", "HEAD"], "Crab source revision"
            ),
            "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            "verifier_sha256": hashlib.sha256(
                (SCRIPT_DIR.parent / "verify-large-repo-rustfs-report.py").read_bytes()
            ).hexdigest(),
        }
        binary_git_sha = crab_build.get("git_sha")
        source_git_sha = self.report["provenance"]["crab_source_revision"]
        self.check(
            "crab-build-matches-source",
            isinstance(binary_git_sha, str)
            and len(binary_git_sha) >= 7
            and binary_git_sha != "unknown"
            and re.fullmatch(r"[0-9a-f]+", binary_git_sha) is not None
            and source_git_sha.startswith(binary_git_sha),
            {
                "binary_git_sha": binary_git_sha,
                "source_git_sha": source_git_sha,
            },
        )
        self.write_report()
        return source_head, base, commits

    def verify_source_unchanged(self) -> None:
        status_record = self.run_git(
            self.source,
            ["status", "--porcelain=v1", "--untracked-files=all"],
            "verify source status unchanged",
        )
        status_hash = hashlib.sha256(self.stdout(status_record).encode()).hexdigest()
        revision = self.git_value(
            self.source,
            ["rev-parse", "HEAD"],
            "verify source revision unchanged",
        )
        self.check(
            "source-checkout-unchanged",
            status_hash == self.report["source"]["status_sha256"]
            and revision == self.report["source"]["revision"],
            {
                "status_hash_matches": status_hash
                == self.report["source"]["status_sha256"],
                "revision_matches": revision == self.report["source"]["revision"],
            },
        )

    def ensure_object_store(self) -> None:
        if self.args.start_rustfs:
            self.run_cmd(
                "start RustFS",
                ["bash", str(START_RUSTFS)],
                REPO_ROOT,
                timeout=10 * 60,
            )
        head = self.run_cmd(
            "verify qualification bucket",
            [
                str(self.aws_bin),
                "--endpoint-url",
                self.args.endpoint_url,
                "s3api",
                "head-bucket",
                "--bucket",
                self.args.bucket,
            ],
            self.run_root,
            check=False,
        )
        if head["exit_code"] != 0 and not self.args.require_existing_bucket:
            self.run_cmd(
                "create qualification bucket",
                [
                    str(self.aws_bin),
                    "--endpoint-url",
                    self.args.endpoint_url,
                    "s3api",
                    "create-bucket",
                    "--bucket",
                    self.args.bucket,
                ],
                self.run_root,
            )
        elif head["exit_code"] != 0:
            raise QualificationError(f"required bucket does not exist: {self.args.bucket}")
        self.probe_cache_service()
        existing = self.list_remote_objects(limit=1)
        self.check(
            "isolated-remote-prefix",
            not existing,
            {"prefix": self.remote_prefix, "existing_objects": len(existing)},
        )
        self.report["provenance"]["object_store"] = {
            "kind": "rustfs",
            "endpoint_url": self.args.endpoint_url,
            "version": self.args.object_store_version,
        }
        self.write_report()

    def cache_service_request(
        self, path: str, *, authenticated: bool
    ) -> tuple[int, bytes, str | None]:
        if not self.cache_service_url:
            return 0, b"", "cache service URL is not configured"

        headers: dict[str, str] = {}
        if authenticated:
            psk = self.env.get(CACHE_SERVICE_PSK_ENV)
            token = self.env.get(CACHE_SERVICE_TOKEN_ENV)
            if psk:
                headers["X-Cache-PSK"] = psk
            elif token:
                headers["Authorization"] = f"Bearer {token}"
        request = urllib.request.Request(
            f"{self.cache_service_url.rstrip('/')}{path}",
            headers=headers,
            method="GET",
        )
        try:
            with urllib.request.urlopen(
                request, timeout=min(30, self.args.timeout)
            ) as response:
                return int(response.status), response.read(), None
        except urllib.error.HTTPError as error:
            return error.code, error.read(), f"HTTP {error.code}"
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            return 0, b"", str(error)

    def probe_cache_service(self) -> None:
        service = self.report["cache_service"]
        if not self.cache_service_url:
            detail = {"configured": False}
            if self.args.require_cache_service:
                self.check("cache-service-configured", False, detail)
            self.write_report()
            return

        health_status, _health_body, health_error = self.cache_service_request(
            "/v1/health", authenticated=False
        )
        capabilities_status, capabilities_body, capabilities_error = (
            self.cache_service_request("/v1/capabilities", authenticated=True)
        )
        try:
            capabilities = json.loads(capabilities_body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            capabilities = {}
        routes = capabilities.get("routes", {}) if isinstance(capabilities, dict) else {}
        immutable_routes = routes.get("immutable", []) if isinstance(routes, dict) else []
        route_patterns = {
            item.get("pattern")
            for item in immutable_routes
            if isinstance(item, dict) and isinstance(item.get("pattern"), str)
        }
        required_routes = {
            "{repo}/packs/pack-{id}.pack",
            "{repo}/packs/pack-{id}.idx",
        }
        capabilities_ok = (
            capabilities_status == 200
            and isinstance(capabilities, dict)
            and capabilities.get("schema") == CACHE_SERVICE_CAPABILITIES_SCHEMA
            and isinstance(routes, dict)
            and routes.get("schema") == CACHE_ROUTE_SCHEMA
            and routes.get("transport_prefix") == "/v1/"
            and required_routes.issubset(route_patterns)
        )
        service.update(
            {
                "configured": True,
                "health_status": health_status,
                "capabilities_status": capabilities_status,
                "capabilities_schema": (
                    capabilities.get("schema")
                    if isinstance(capabilities, dict)
                    else None
                ),
                "route_schema": routes.get("schema") if isinstance(routes, dict) else None,
            }
        )
        if self.args.require_cache_service:
            self.check(
                "cache-service-configured",
                True,
                {"url": self.cache_service_url},
            )
            self.check(
                "cache-service-healthy",
                health_status == 200,
                {"status": health_status, "error": health_error},
            )
            self.check(
                "cache-service-capabilities",
                capabilities_ok,
                {
                    "status": capabilities_status,
                    "schema": service["capabilities_schema"],
                    "route_schema": service["route_schema"],
                    "error": capabilities_error,
                },
            )
        self.write_report()

    def collect_cache_service_stats(self) -> None:
        if not self.cache_service_url:
            return
        status, body, error = self.cache_service_request(
            "/v1/admin/stats", authenticated=True
        )
        try:
            payload = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            payload = {}
        traffic = payload.get("traffic", {}) if isinstance(payload, dict) else {}
        by_object_type = (
            traffic.get("by_object_type", {}) if isinstance(traffic, dict) else {}
        )
        pack = by_object_type.get("pack", {}) if isinstance(by_object_type, dict) else {}
        fields = (
            "cache_hits",
            "cache_misses",
            "origin_fetches",
            "origin_head_requests",
            "bytes_served_from_cache",
            "bytes_served_from_origin",
            "bytes_served_total",
            "push_warming_writes",
            "push_warming_bytes",
        )

        def counter(source: Any, field: str) -> int:
            value = source.get(field) if isinstance(source, dict) else None
            return value if isinstance(value, int) and not isinstance(value, bool) else 0

        pack_stats = {field: counter(pack, field) for field in fields}
        pack_stats["read_requests"] = sum(
            pack_stats[field]
            for field in (
                "cache_hits",
                "cache_misses",
                "origin_head_requests",
            )
        )
        service = self.report["cache_service"]
        service["stats"] = {
            "status": status,
            "error": error,
            "pack": pack_stats,
        }
        if self.args.require_cache_service:
            stats_ok = status == 200 and isinstance(payload, dict) and isinstance(traffic, dict)
            self.check(
                "cache-service-admin-stats",
                stats_ok,
                {"status": status, "error": error},
            )
            self.check(
                "cache-service-pack-traffic",
                stats_ok and pack_stats["read_requests"] > 0,
                {"pack": pack_stats},
            )
        self.write_report()

    def list_remote_objects(self, limit: int | None = None) -> list[dict[str, Any]]:
        objects: list[dict[str, Any]] = []
        token: str | None = None
        while True:
            command = [
                str(self.aws_bin),
                "--endpoint-url",
                self.args.endpoint_url,
                "s3api",
                "list-objects-v2",
                "--bucket",
                self.args.bucket,
                "--prefix",
                f"{self.remote_prefix}/",
                "--max-keys",
                str(min(1_000, limit or 1_000)),
                "--output",
                "json",
            ]
            if token:
                command.extend(["--continuation-token", token])
            record = self.run_cmd("list qualification objects", command, self.run_root)
            payload = json.loads(self.stdout(record) or "{}")
            objects.extend(payload.get("Contents", []))
            if limit is not None and len(objects) >= limit:
                return objects[:limit]
            if not payload.get("IsTruncated"):
                return objects
            token = payload.get("NextContinuationToken")
            if not token:
                raise QualificationError("truncated object listing has no continuation token")

    def store_snapshot(self, stage: str) -> None:
        objects = self.list_remote_objects()
        packs = [
            item
            for item in objects
            if str(item.get("Key", "")).startswith(f"{self.remote_prefix}/packs/pack-")
            and str(item.get("Key", "")).endswith(".pack")
        ]
        snapshot = {
            "stage": stage,
            "objects": len(objects),
            "bytes": sum(int(item.get("Size", 0)) for item in objects),
            "physical_packs": len(packs),
            "physical_pack_bytes": sum(int(item.get("Size", 0)) for item in packs),
        }
        self.report["store_snapshots"].append(snapshot)
        self.write_report()

    def active_pack_snapshot(self, stage: str) -> None:
        record = self.run_crab(
            self.replay_repo,
            ["repack", "--dry-run", "--json"],
            f"active pack snapshot {stage}",
        )
        payload = json.loads(self.stdout(record))
        data = payload.get("data", payload)
        self.report["stages"][f"pack_inventory_{stage}"] = {
            "duration_ms": record["duration_ms"],
            "active_packs": int(data["packs_before"]),
            "active_pack_bytes": int(data["bytes_before"]),
        }
        self.write_report()

    def acceleration_snapshot(self, stage: str) -> None:
        owner_runs: list[dict[str, Any]] = []
        owner_snapshots: list[dict[str, Any]] = []
        actions: list[str] = []
        max_passes = BASE_GENERATION_OWNER_PASSES
        attempt = 0
        while attempt < max_passes:
            attempt += 1
            owner = self.run_crab(
                self.replay_repo,
                ["metadb", "owner", "--once", "--jsonl"],
                f"generation owner {stage} pass {attempt}",
                timeout=self.args.clone_timeout,
                extra_env={"CRAB_LOG": QUALIFICATION_DEBUG_LOG},
            )
            owner_runs.append(owner)
            lines = [line for line in self.stdout(owner).splitlines() if line.strip()]
            if not lines:
                raise QualificationError("generation owner emitted no JSONL snapshot")
            payload = json.loads(lines[-1])
            data = payload.get("data", payload)
            if not isinstance(data, dict):
                raise QualificationError("generation owner snapshot data is not an object")
            owner_snapshots.append(data)
            for field in ("active_packs", "geometric_repack_packs"):
                value = data.get(field)
                if (
                    isinstance(value, int)
                    and not isinstance(value, bool)
                    and value >= 0
                ):
                    max_passes = max(
                        max_passes,
                        BASE_GENERATION_OWNER_PASSES
                        + GENERATION_OWNER_PASSES_PER_PACK * value,
                    )
            action = str(data.get("action", ""))
            actions.append(action)
            if action == "none":
                break
        else:
            raise QualificationError(
                "generation owner did not converge after "
                f"{max_passes} passes: {actions}"
            )
        locator_sweeps: list[dict[str, Any]] = []
        for index, snapshot in enumerate(owner_snapshots):
            raw_sweep = snapshot.get("locator_sweep")
            if not isinstance(raw_sweep, dict):
                raise QualificationError(
                    f"generation owner pass {index + 1} omitted locator sweep telemetry"
                )
            sweep: dict[str, Any] = {"action": str(snapshot.get("action", ""))}
            if not sweep["action"]:
                raise QualificationError(
                    f"generation owner pass {index + 1} omitted maintenance action"
                )
            for counter in (
                "object_rows_scanned",
                "object_rows_deleted",
                "pack_rows_scanned",
                "pack_rows_deleted",
            ):
                value = raw_sweep.get(counter)
                if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                    raise QualificationError(
                        f"generation owner pass {index + 1} has invalid locator sweep {counter}"
                    )
                sweep[counter] = value
            locator_sweeps.append(sweep)
        doctor = self.run_crab(
            self.replay_repo,
            ["doctor", "--metadb", "--json"],
            f"acceleration diagnosis {stage}",
            timeout=self.args.clone_timeout,
        )
        payload = json.loads(self.stdout(doctor))
        data = payload.get("data", payload)
        acceleration = data.get("acceleration")
        if not isinstance(acceleration, dict):
            raise QualificationError("doctor --metadb JSON is missing acceleration state")
        self.report["stages"][f"visibility_owner_{stage}"] = {
            "duration_ms": sum(run["duration_ms"] for run in owner_runs),
            "passes": len(owner_runs),
            "actions": actions,
            "resources": {
                "user_cpu_ms": sum(
                    run["resources"]["user_cpu_ms"] for run in owner_runs
                ),
                "system_cpu_ms": sum(
                    run["resources"]["system_cpu_ms"] for run in owner_runs
                ),
                "children_max_rss": max(
                    run["resources"]["children_max_rss"] for run in owner_runs
                ),
                "children_max_rss_unit": "bytes",
            },
            "telemetry": {
                key: sum(run["telemetry"].get(key, 0) for run in owner_runs)
                for key in owner_runs[0]["telemetry"]
            },
            "visibility_states": [
                str(snapshot.get("visibility", "")) for snapshot in owner_snapshots
            ],
            "maintenance_bytes_read": sum(
                int(snapshot.get("maintenance_bytes_read", 0))
                for snapshot in owner_snapshots
            ),
            "maintenance_bytes_written": sum(
                int(snapshot.get("maintenance_bytes_written", 0))
                for snapshot in owner_snapshots
            ),
            "locator_sweep": locator_sweeps,
        }
        self.report["stages"][f"acceleration_{stage}"] = {
            "duration_ms": doctor["duration_ms"],
            "manifest_generation": acceleration.get("manifest_generation"),
            "generation_receipt_valid": acceleration.get("generation_receipt_valid"),
            "ref_registry_repo_complete": acceleration.get(
                "ref_registry_repo_complete"
            ),
            "locator_available": acceleration.get("git_locator_index_available"),
            "locator_generation": acceleration.get("git_locator_covered_generation"),
            "locator_pack_index_hash": acceleration.get(
                "git_locator_covered_pack_index_hash"
            ),
            "visibility_generation": acceleration.get(
                "git_visibility_covered_generation"
            ),
            "visibility_available": acceleration.get(
                "git_visibility_index_available"
            ),
            "visibility_pack_index_hash": acceleration.get(
                "git_visibility_covered_pack_index_hash"
            ),
            "visibility_current": acceleration.get(
                "git_visibility_coverage_current"
            ),
            "commit_graph_available": acceleration.get(
                "git_commit_graph_available"
            ),
            "commit_graph_commits": acceleration.get("git_commit_graph_commits"),
            "commit_graph_layers": acceleration.get("git_commit_graph_layers"),
            "commit_graph_current": acceleration.get("git_commit_graph_current"),
            "repair_required": acceleration.get("repair_required"),
            "notes": acceleration.get("notes", []),
        }
        state = self.report["stages"][f"acceleration_{stage}"]
        self.check(
            f"acceleration-current-{stage}",
            state["manifest_generation"] is not None
            and state["ref_registry_repo_complete"] is True
            and state["locator_available"] is True
            and state["locator_generation"] == state["manifest_generation"]
            and state["visibility_available"] is True
            and state["visibility_generation"] == state["manifest_generation"]
            and state["locator_pack_index_hash"]
            == state["visibility_pack_index_hash"]
            and state["visibility_current"] is True
            and state["commit_graph_available"] is True
            and state["commit_graph_current"] is True,
            state,
        )
        self.write_report()

    def setup_replay(self, base: str) -> None:
        self.run_git(
            self.run_root,
            ["clone", "--shared", "--no-checkout", str(self.source), str(self.replay_repo)],
            "create read-only source clone",
            timeout=2 * 60 * 60,
        )
        self.run_git(self.replay_repo, ["remote", "remove", "origin"], "remove source remote")
        self.run_git(self.replay_repo, ["checkout", "--detach", base], "checkout replay base")
        self.run_git(self.replay_repo, ["branch", "-f", "main", base], "create replay main")
        self.run_git(self.replay_repo, ["switch", "main"], "switch replay main")
        self.run_crab(
            self.replay_repo,
            ["init", self.remote_url],
            "initialize Crab remote",
        )

    def push_commit(self, commit: str, ordinal: int, name: str) -> dict[str, Any]:
        record = self.run_crab(
            self.replay_repo,
            ["push", "--jsonl", "origin", f"{commit}:refs/heads/main"],
            name,
            timeout=self.args.push_timeout,
        )
        self.report["pushes"].append(
            {
                "ordinal": ordinal,
                "commit": commit,
                "duration_ms": record["duration_ms"],
                "resources": record["resources"],
                "telemetry": record["telemetry"],
            }
        )
        self.write_report()
        return record

    def clone(
        self,
        name: str,
        target: Path,
        options: list[str],
        *,
        fsck: bool,
        remove_after: bool = False,
    ) -> dict[str, Any]:
        record = self.run_git(
            self.run_root,
            ["-c", "protocol.version=2", "clone", *options, self.remote_url, str(target)],
            name,
            timeout=self.args.clone_timeout,
            extra_env={"CRAB_LOG": QUALIFICATION_DEBUG_LOG},
        )
        if fsck:
            self.run_git(target, ["fsck", "--full"], f"{name} fsck", timeout=2 * 60 * 60)
        stage = {
            "duration_ms": record["duration_ms"],
            "resources": record["resources"],
            "telemetry": record["telemetry"],
            "odb_bytes": sum(
                path.stat().st_size
                for path in (target / ".git" / "objects").rglob("*")
                if path.is_file()
            ),
            "worktree_retained_for_correctness": not remove_after,
        }
        self.report["stages"][name] = stage
        self.write_report()
        if remove_after:
            shutil.rmtree(target)
        return stage

    def clone_fanout(self, name: str, count: int) -> None:
        if count == 0:
            return
        barrier = threading.Barrier(count)

        def worker(ordinal: int) -> dict[str, Any]:
            target = self.clone_root / f"{name}-{ordinal:03d}"
            barrier.wait(timeout=60)
            clone = self.run_git(
                self.run_root,
                [
                    "-c",
                    "protocol.version=2",
                    "clone",
                    "--no-checkout",
                    "--single-branch",
                    "--branch",
                    "main",
                    self.remote_url,
                    str(target),
                ],
                f"{name} clone {ordinal:03d}",
                timeout=self.args.clone_timeout,
                extra_env={"CRAB_LOG": QUALIFICATION_DEBUG_LOG},
            )
            self.run_git(
                target,
                ["fsck", "--full"],
                f"{name} fsck {ordinal:03d}",
                timeout=2 * 60 * 60,
            )
            shutil.rmtree(target)
            return clone

        started = time.monotonic()
        runs: list[dict[str, Any]] = []
        with ThreadPoolExecutor(max_workers=count) as executor:
            futures = [executor.submit(worker, ordinal) for ordinal in range(1, count + 1)]
            for future in as_completed(futures):
                runs.append(future.result())
        durations = [int(run["duration_ms"]) for run in runs]
        producers = sum(
            1 for run in runs if int(run["telemetry"].get("pack_generation_ms", 0)) > 0
        )
        hits = sum(int(run["telemetry"].get("cache_hits", 0)) for run in runs)
        misses = sum(int(run["telemetry"].get("cache_misses", 0)) for run in runs)
        origin_requests = sum(
            int(run["telemetry"].get("storage_requests", 0)) for run in runs
        )
        self.report["stages"][name] = {
            "duration_ms": int((time.monotonic() - started) * 1_000),
            "clients": count,
            "successful_fsck": len(runs),
            "generated_pack_producers": producers,
            "cache_hits": hits,
            "cache_misses": misses,
            "cache_hit_rate": hits / max(1, hits + misses),
            "origin_requests": origin_requests,
            "median_client_ms": percentile(durations, 0.50),
            "p95_client_ms": percentile(durations, 0.95),
            "p99_client_ms": percentile(durations, 0.99),
        }
        self.check(f"{name}-all-fsck", len(runs) == count, {"clients": count})
        if name == "cold_clone_fanout":
            self.check(
                "cold-clone-generated-pack-producers",
                producers <= 2,
                {"clients": count, "producers": producers},
            )
        if name == "warm_clone_fanout":
            self.check(
                "warm-clone-generated-pack-cache-hit-rate",
                hits + misses >= count and hits / max(1, hits + misses) >= 0.90,
                {"clients": count, "hits": hits, "misses": misses},
            )
            cold = self.report["stages"].get("cold_clone_fanout")
            if isinstance(cold, dict):
                cold_requests = int(cold.get("origin_requests", 0))
                self.check(
                    "warm-clone-origin-request-reduction",
                    cold_requests > 0 and origin_requests <= cold_requests * 0.20,
                    {"cold": cold_requests, "warm": origin_requests},
                )
        self.write_report()

    def prepare_fetch_fanout(self, checkpoint: int, expected: str) -> None:
        count = self.args.fetch_fanout
        barrier = threading.Barrier(count)

        def worker(ordinal: int) -> dict[str, Any]:
            target = self.fetch_root / f"client-{ordinal:03d}"
            barrier.wait(timeout=60)
            record = self.run_git(
                self.run_root,
                [
                    "-c",
                    "protocol.version=2",
                    "clone",
                    "--depth=1",
                    "--no-checkout",
                    "--single-branch",
                    "--branch",
                    "main",
                    self.remote_url,
                    str(target),
                ],
                f"fetch fanout seed clone {ordinal:03d}",
                check=False,
                timeout=self.args.clone_timeout,
                extra_env={"CRAB_LOG": QUALIFICATION_DEBUG_LOG},
            )
            if record["exit_code"] != 0 and target.exists():
                shutil.rmtree(target)
            return {
                "ordinal": ordinal,
                "exit_code": record["exit_code"],
                "duration_ms": record["duration_ms"],
                "failure_category": "ok" if record["exit_code"] == 0 else "clone_failed",
                "telemetry": record["telemetry"],
            }

        started = time.monotonic()
        runs: list[dict[str, Any]] = []
        with ThreadPoolExecutor(max_workers=count) as executor:
            futures = [executor.submit(worker, ordinal) for ordinal in range(1, count + 1)]
            for future in as_completed(futures):
                runs.append(future.result())
        durations = [int(run["duration_ms"]) for run in runs]
        successful = sum(run["failure_category"] == "ok" for run in runs)
        producers = sum(
            int(run["telemetry"].get("pack_generation_ms", 0)) > 0 for run in runs
        )
        cache_hits = sum(int(run["telemetry"].get("cache_hits", 0)) for run in runs)
        cache_misses = sum(
            int(run["telemetry"].get("cache_misses", 0)) for run in runs
        )
        origin_requests = sum(
            int(run["telemetry"].get("storage_requests", 0)) for run in runs
        )
        self.report["team_load"]["fetch_seed"] = {
            "checkpoint": checkpoint,
            "tip": expected,
            "clients": count,
            "successful_clones": successful,
            "generated_pack_producers": producers,
            "cache_hits": cache_hits,
            "cache_misses": cache_misses,
            "origin_requests": origin_requests,
            "duration_ms": int((time.monotonic() - started) * 1_000),
            "median_client_ms": percentile(durations, 0.50),
            "p95_client_ms": percentile(durations, 0.95),
            "p99_client_ms": percentile(durations, 0.99),
            "results": sorted(
                (
                    {key: value for key, value in run.items() if key != "telemetry"}
                    for run in runs
                ),
                key=lambda run: int(run["ordinal"]),
            ),
        }
        self.check(
            "concurrent-fetch-seed-clones",
            len(runs) == count and successful == count,
            {"clients": count, "successful_clones": successful},
        )
        self.check(
            "concurrent-fetch-seed-generated-pack-producers",
            1 <= producers <= 2,
            {"clients": count, "producers": producers},
        )
        self.write_report()

    def concurrent_incremental_fetches(self, expected: str) -> None:
        count = self.args.fetch_fanout
        barrier = threading.Barrier(count)
        seed = self.report["team_load"].get("fetch_seed", {})
        self.check(
            "concurrent-incremental-fetch-span",
            seed.get("checkpoint", self.args.replay_count) < self.args.replay_count
            and seed.get("tip") != expected,
            {
                "from_checkpoint": seed.get("checkpoint"),
                "to_checkpoint": self.args.replay_count,
                "from_tip": seed.get("tip"),
                "to_tip": expected,
            },
        )

        def worker(ordinal: int) -> dict[str, Any]:
            target = self.fetch_root / f"client-{ordinal:03d}"
            barrier.wait(timeout=60)
            fetch = self.run_git(
                target,
                ["fetch", "origin", "refs/heads/main:refs/remotes/origin/main"],
                f"concurrent incremental fetch {ordinal:03d}",
                check=False,
                timeout=self.args.clone_timeout,
                extra_env={"CRAB_LOG": QUALIFICATION_DEBUG_LOG},
            )
            if fetch["exit_code"] != 0:
                return {
                    "ordinal": ordinal,
                    "exit_code": fetch["exit_code"],
                    "fetch_exit_code": fetch["exit_code"],
                    "fsck_exit_code": None,
                    "tip_matches": False,
                    "duration_ms": fetch["duration_ms"],
                    "failure_category": "fetch_failed",
                }
            fsck = self.run_git(
                target,
                ["fsck", "--full"],
                f"concurrent incremental fetch {ordinal:03d} fsck",
                check=False,
                timeout=2 * 60 * 60,
            )
            tip = self.run_git(
                target,
                ["rev-parse", "refs/remotes/origin/main"],
                f"concurrent incremental fetch {ordinal:03d} tip",
                check=False,
            )
            return {
                "ordinal": ordinal,
                "exit_code": fetch["exit_code"],
                "fetch_exit_code": fetch["exit_code"],
                "fsck_exit_code": fsck["exit_code"],
                "tip_matches": tip["exit_code"] == 0
                and self.stdout(tip).strip() == expected,
                "duration_ms": fetch["duration_ms"],
                "failure_category": (
                    "ok"
                    if fsck["exit_code"] == 0
                    and tip["exit_code"] == 0
                    and self.stdout(tip).strip() == expected
                    else "verification_failed"
                ),
            }

        started = time.monotonic()
        results: list[dict[str, Any]] = []
        with ThreadPoolExecutor(max_workers=count) as executor:
            futures = [executor.submit(worker, ordinal) for ordinal in range(1, count + 1)]
            for future in as_completed(futures):
                results.append(future.result())
        results.sort(key=lambda result: int(result["ordinal"]))
        durations = [int(result["duration_ms"]) for result in results]
        successful = sum(result["failure_category"] == "ok" for result in results)
        self.report["team_load"]["concurrent_incremental_fetches"] = {
            "from_checkpoint": seed.get("checkpoint"),
            "to_checkpoint": self.args.replay_count,
            "from_tip": seed.get("tip"),
            "to_tip": expected,
            "clients": count,
            "successful": successful,
            "failed": count - successful,
            "duration_ms": int((time.monotonic() - started) * 1_000),
            "median_client_ms": percentile(durations, 0.50),
            "p95_client_ms": percentile(durations, 0.95),
            "p99_client_ms": percentile(durations, 0.99),
            "results": results,
        }
        self.check(
            "concurrent-incremental-fetches",
            len(results) == count and successful == count,
            {"clients": count, "successful": successful},
        )
        self.write_report()

    def create_team_push_client(
        self, kind: str, ordinal: int, base: str
    ) -> dict[str, Any]:
        target = self.team_root / f"{kind}-{ordinal:03d}"
        self.run_git(
            self.run_root,
            ["clone", "--shared", "--no-checkout", str(self.replay_repo), str(target)],
            f"create {kind} push client {ordinal:03d}",
            timeout=2 * 60 * 60,
        )
        branch = f"team-{kind}-{ordinal:03d}"
        self.run_git(target, ["checkout", "-b", branch, base], f"checkout {kind} push client")
        self.run_git(
            target,
            ["remote", "remove", "origin"],
            f"remove {kind} push client source remote",
        )
        self.run_crab(target, ["init", self.remote_url], f"initialize {kind} push client")
        filename = target / f"team-{kind}-{ordinal:03d}.txt"
        filename.write_text(
            f"Crab large-team qualification change {kind} {ordinal:03d}\n",
            encoding="utf-8",
        )
        commit_date = f"2000-01-01T00:00:{ordinal:02d}Z"
        commit_env = {
            "GIT_AUTHOR_DATE": commit_date,
            "GIT_COMMITTER_DATE": commit_date,
        }
        self.run_git(target, ["add", filename.name], f"stage {kind} push client")
        self.run_git(
            target,
            ["commit", "-m", f"team {kind} change {ordinal:03d}"],
            f"commit {kind} push client",
            extra_env=commit_env,
        )
        commit = self.git_value(target, ["rev-parse", "HEAD"], f"resolve {kind} push client")
        return {"ordinal": ordinal, "target": target, "commit": commit}

    @staticmethod
    def push_failure_category(record: dict[str, Any], output: str) -> str:
        if record["exit_code"] == 0:
            return "accepted"
        lowered = output.lower()
        if "crab-e0012" in lowered or "push lock" in lowered:
            return "push_lock"
        if "crab-e0017" in lowered or "non-fast-forward" in lowered:
            return "non_fast_forward"
        if "crab-e0010" in lowered or "cas conflict" in lowered:
            return "cas_conflict"
        return "unexpected"

    def push_client_fanout(
        self,
        name: str,
        clients: list[dict[str, Any]],
        destination: str | Callable[[dict[str, Any]], str],
        *,
        exactly_one_success: bool,
    ) -> list[dict[str, Any]]:
        count = len(clients)
        barrier = threading.Barrier(count)

        def worker(client: dict[str, Any]) -> dict[str, Any]:
            barrier.wait(timeout=60)
            destination_for_client = (
                destination(client) if callable(destination) else destination
            )
            record = self.run_crab(
                client["target"],
                [
                    "push",
                    "--jsonl",
                    "origin",
                    f"{client['commit']}:{destination_for_client}",
                ],
                f"{name} push {client['ordinal']:03d}",
                check=False,
                timeout=self.args.push_timeout,
            )
            output = self.stdout(record) + self.stderr(record)
            return {
                "ordinal": client["ordinal"],
                "duration_ms": record["duration_ms"],
                "exit_code": record["exit_code"],
                "failure_category": self.push_failure_category(record, output),
                "commit": client["commit"],
            }

        started = time.monotonic()
        results: list[dict[str, Any]] = []
        with ThreadPoolExecutor(max_workers=count) as executor:
            futures = [executor.submit(worker, client) for client in clients]
            for future in as_completed(futures):
                results.append(future.result())
        results.sort(key=lambda result: int(result["ordinal"]))
        accepted = [result for result in results if result["failure_category"] == "accepted"]
        allowed_rejections = {"push_lock", "non_fast_forward", "cas_conflict"}
        rejected = [
            result
            for result in results
            if result["failure_category"] in allowed_rejections
        ]
        durations = [int(result["duration_ms"]) for result in results]
        stage = {
            "clients": count,
            "successful": len(accepted),
            "rejected": len(rejected),
            "unexpected_failures": count - len(accepted) - len(rejected),
            "duration_ms": int((time.monotonic() - started) * 1_000),
            "median_client_ms": percentile(durations, 0.50),
            "p95_client_ms": percentile(durations, 0.95),
            "p99_client_ms": percentile(durations, 0.99),
            "results": results,
        }
        self.report["team_load"][name] = stage
        if exactly_one_success:
            ok = len(accepted) == 1 and len(rejected) == count - 1 and not stage[
                "unexpected_failures"
            ]
        else:
            ok = len(accepted) == count and not stage["unexpected_failures"]
        self.check(
            f"{name}-outcomes",
            ok,
            {
                "clients": count,
                "successful": len(accepted),
                "rejected": len(rejected),
                "unexpected_failures": stage["unexpected_failures"],
            },
        )
        self.write_report()
        return results

    def run_team_load(self, source_head: str) -> None:
        self.report["team_load"]["enabled"] = True
        self.report["team_load"]["fetch_fanout"] = self.args.fetch_fanout
        self.report["team_load"]["independent_pushes"] = self.args.independent_pushes
        self.report["team_load"]["contended_pushes"] = self.args.contended_pushes
        self.concurrent_incremental_fetches(source_head)

        independent_clients = [
            self.create_team_push_client("independent", ordinal, source_head)
            for ordinal in range(1, self.args.independent_pushes + 1)
        ]
        independent_ref = "refs/heads/team/independent"
        independent_results = self.push_client_fanout(
            "independent_ref_pushes",
            independent_clients,
            lambda client: f"{independent_ref}/client/{client['ordinal']:03d}",
            exactly_one_success=False,
        )
        refs = self.remote_refs()
        missing_independent = [
            result["ordinal"]
            for result in independent_results
            if refs.get(f"{independent_ref}/client/{result['ordinal']:03d}")
            != result["commit"]
        ]
        self.check(
            "independent-ref-pushes-preserved",
            not missing_independent,
            {"clients": len(independent_results), "missing": missing_independent},
        )

        contended_ref = "refs/heads/team/contended"
        self.run_crab(
            self.replay_repo,
            ["push", "--jsonl", "origin", f"{source_head}:{contended_ref}"],
            "seed same-ref contention",
            timeout=self.args.push_timeout,
        )
        contended_clients = [
            self.create_team_push_client("contended", ordinal, source_head)
            for ordinal in range(1, self.args.contended_pushes + 1)
        ]
        contended_results = self.push_client_fanout(
            "same_ref_pushes",
            contended_clients,
            contended_ref,
            exactly_one_success=True,
        )
        accepted = [
            result for result in contended_results if result["failure_category"] == "accepted"
        ]
        refs = self.remote_refs()
        self.check(
            "same-ref-winner-published",
            len(accepted) == 1 and refs.get(contended_ref) == accepted[0]["commit"],
            {
                "accepted": len(accepted),
                "remote_tip_matches_winner": len(accepted) == 1
                and refs.get(contended_ref) == accepted[0]["commit"],
            },
        )
        self.write_report()

    def incremental_fetch(self, checkpoint: int, expected: str) -> None:
        record = self.run_git(
            self.incremental_clone,
            ["fetch", "origin", "refs/heads/main:refs/remotes/origin/main"],
            f"incremental fetch after {checkpoint} pushes",
            timeout=self.args.clone_timeout,
            extra_env={"CRAB_LOG": QUALIFICATION_DEBUG_LOG},
        )
        actual = self.git_value(
            self.incremental_clone,
            ["rev-parse", "refs/remotes/origin/main"],
            f"incremental fetch tip after {checkpoint}",
        )
        self.check(
            f"incremental-fetch-tip-{checkpoint}",
            actual == expected,
            {"expected": expected, "actual": actual},
        )
        self.report["stages"][f"incremental_fetch_{checkpoint}"] = {
            "duration_ms": record["duration_ms"],
            "resources": record["resources"],
            "telemetry": record["telemetry"],
        }
        self.write_report()

    def replay(self, base: str, commits: list[str]) -> None:
        initial = self.push_commit(base, 0, "initial import")
        self.report["stages"]["initial_import"] = {
            "duration_ms": initial["duration_ms"],
            "resources": initial["resources"],
            "telemetry": initial["telemetry"],
        }
        self.acceleration_snapshot("seed")
        self.active_pack_snapshot("seed")
        self.store_snapshot("seed")
        self.clone(
            "incremental_seed_clone",
            self.incremental_clone,
            ["--single-branch", "--branch", "main"],
            fsck=False,
        )
        checkpoints = {
            checkpoint
            for checkpoint in (1, 10, 100, self.args.replay_count)
            if checkpoint <= self.args.replay_count
        }
        fetch_seed_checkpoint = 100 if self.args.replay_count > 100 else 10
        for ordinal, commit in enumerate(commits, start=1):
            self.push_commit(commit, ordinal, f"replay push {ordinal:04d}")
            if ordinal in checkpoints:
                self.acceleration_snapshot(str(ordinal))
                self.incremental_fetch(ordinal, commit)
                self.active_pack_snapshot(str(ordinal))
                self.store_snapshot(str(ordinal))
            if self.args.team_load and ordinal == fetch_seed_checkpoint:
                # Seed fanout only after checkpoint maintenance has published
                # the generation required by protocol-v2 readers. Starting it
                # before the owner run makes the harness wait on clients that
                # are, correctly, waiting on that same owner run.
                self.prepare_fetch_fanout(ordinal, commit)

    def final_clones(self) -> Path:
        cold = self.clone_root / "full-cold"
        warm = self.clone_root / "full-warm"
        filtered = self.clone_root / "blob-none"
        depth_one = self.clone_root / "depth-1"
        depth_ten = self.clone_root / "depth-10"
        depth_hundred = self.clone_root / "depth-100"
        depth_thousand = self.clone_root / "depth-1000"
        self.clone("full_clone_cold", cold, ["--branch", "main"], fsck=True)
        self.clone(
            "full_clone_warm",
            warm,
            ["--branch", "main"],
            fsck=True,
            remove_after=True,
        )
        blob_none = self.clone(
            "blob_none_clone",
            filtered,
            ["--filter=blob:none", "--no-checkout", "--single-branch", "--branch", "main"],
            fsck=False,
            remove_after=True,
        )
        blob_none_telemetry = blob_none["telemetry"]
        metadata_lookup_events = sum(
            int(blob_none_telemetry.get(field, 0))
            for field in ("locator_ordinal_metadata", "locator_ordinal_metadata_scan")
        )
        self.check(
            "blob-none-ordinal-metadata-lookup",
            metadata_lookup_events > 0,
            {"metadata_lookup_events": metadata_lookup_events},
        )
        self.clone(
            "depth_1_clone",
            depth_one,
            ["--depth=1", "--single-branch", "--branch", "main"],
            fsck=False,
            remove_after=True,
        )
        self.clone(
            "depth_10_clone",
            depth_ten,
            ["--depth=10", "--single-branch", "--branch", "main"],
            fsck=False,
            remove_after=True,
        )
        self.clone(
            "depth_100_clone",
            depth_hundred,
            ["--depth=100", "--single-branch", "--branch", "main"],
            fsck=False,
            remove_after=True,
        )
        self.clone(
            "depth_1000_clone",
            depth_thousand,
            ["--depth=1000", "--single-branch", "--branch", "main"],
            fsck=False,
            remove_after=True,
        )
        self.clone_fanout("cold_clone_fanout", self.args.cold_clone_fanout)
        self.clone_fanout("warm_clone_fanout", self.args.warm_clone_fanout)
        self.run_git(
            self.incremental_clone,
            ["fsck", "--full"],
            "incremental clone final fsck",
            timeout=2 * 60 * 60,
        )
        return cold

    def remote_refs(self) -> dict[str, str]:
        record = self.run_git(
            self.run_root,
            ["ls-remote", self.remote_url],
            "advertised remote refs",
            extra_env={"CRAB_LOG": "crab=info,crab_remote_git=info"},
        )
        refs: dict[str, str] = {}
        for line in self.stdout(record).splitlines():
            fields = line.split()
            if len(fields) != 2 or not OID_RE.fullmatch(fields[0]):
                raise QualificationError(f"invalid ls-remote line: {line!r}")
            refs[fields[1]] = fields[0]
        return refs

    def deterministic_object_sample(self, revision: str) -> list[str]:
        record = self.run_git(
            self.source,
            ["rev-list", "--objects", revision],
            "enumerate source objects",
            timeout=2 * 60 * 60,
        )
        heap: list[tuple[int, str]] = []
        for line in self.stdout(record).splitlines():
            oid = line.split(" ", 1)[0]
            if not OID_RE.fullmatch(oid):
                continue
            score = int.from_bytes(hashlib.sha256(oid.encode()).digest(), "big")
            entry = (-score, oid)
            if len(heap) < self.args.sample_size:
                heapq.heappush(heap, entry)
            elif entry > heap[0]:
                heapq.heapreplace(heap, entry)
        sample = sorted(oid for _score, oid in heap)
        self.check(
            "deterministic-object-sample-size",
            len(sample) == self.args.sample_size,
            {"expected": self.args.sample_size, "actual": len(sample)},
        )
        return sample

    def batch_check(self, repo: Path, sample: list[str], name: str) -> list[str]:
        record = self.run_git(
            repo,
            ["cat-file", "--batch-check=%(objectname) %(objecttype) %(objectsize)"],
            name,
            input_data=("\n".join(sample) + "\n").encode(),
            timeout=2 * 60 * 60,
        )
        return [line for line in self.stdout(record).splitlines() if line]

    def clone_advertised_refs(
        self, clone: Path, advertised: dict[str, str]
    ) -> dict[str, str | None]:
        record = self.run_git(
            clone,
            [
                "for-each-ref",
                "--format=%(refname) %(objectname) %(*objectname)",
                "refs/remotes/origin",
                "refs/tags",
                "refs/crab-verify",
            ],
            "clone advertised refs",
        )
        refs: dict[str, str] = {}
        peeled: dict[str, str] = {}
        for line in self.stdout(record).splitlines():
            fields = line.split(maxsplit=2)
            if len(fields) < 2 or not OID_RE.fullmatch(fields[1]):
                raise QualificationError(f"invalid clone ref line: {line!r}")
            local_ref, oid = fields[0], fields[1]
            refs[local_ref] = oid
            if len(fields) == 3 and OID_RE.fullmatch(fields[2]):
                peeled[local_ref] = fields[2]

        mapped: dict[str, str] = {}
        for local_ref, oid in refs.items():
            if local_ref.startswith("refs/remotes/origin/"):
                suffix = local_ref.removeprefix("refs/remotes/origin/")
                if suffix != "HEAD":
                    mapped[f"refs/heads/{suffix}"] = oid
            elif local_ref.startswith("refs/crab-verify/"):
                suffix = local_ref.removeprefix("refs/crab-verify/")
                mapped[f"refs/{suffix}"] = oid
                if local_ref in peeled:
                    mapped[f"refs/{suffix}^{{}}"] = peeled[local_ref]
            elif local_ref.startswith("refs/tags/"):
                mapped[local_ref] = oid
                if local_ref in peeled:
                    mapped[f"{local_ref}^{{}}"] = peeled[local_ref]

        head = self.git_value(clone, ["rev-parse", "HEAD"], "clone advertised HEAD")
        return {
            name: head if name == "HEAD" else mapped.get(name)
            for name in advertised
        }

    def verify_correctness(self, source_head: str, full_clone: Path) -> None:
        refs = self.remote_refs()
        main = refs.get("refs/heads/main")
        head = refs.get("HEAD")
        self.run_git(
            full_clone,
            ["fetch", "origin", "+refs/*:refs/crab-verify/*"],
            "clone all advertised refs",
            timeout=self.args.clone_timeout,
            extra_env={"CRAB_LOG": QUALIFICATION_DEBUG_LOG},
        )
        clone_refs = self.clone_advertised_refs(full_clone, refs)
        self.check(
            "advertised-refs-match-source",
            main == source_head
            and head in {None, source_head}
            and clone_refs == refs,
            {
                "expected_main": source_head,
                "actual_main": main,
                "head": head,
                "advertised_refs": refs,
                "clone_refs": clone_refs,
            },
        )
        full_tip = self.git_value(full_clone, ["rev-parse", "HEAD"], "full clone HEAD")
        incremental_tip = self.git_value(
            self.incremental_clone,
            ["rev-parse", "refs/remotes/origin/main"],
            "incremental clone final tip",
        )
        self.check(
            "clone-tips-match-source",
            full_tip == source_head and incremental_tip == source_head,
            {
                "source": source_head,
                "full_clone": full_tip,
                "incremental_clone": incremental_tip,
            },
        )
        sample = self.deterministic_object_sample(source_head)
        source_rows = self.batch_check(self.source, sample, "source object sample")
        clone_rows = self.batch_check(full_clone, sample, "clone object sample")
        self.check(
            "sampled-objects-byte-identical",
            source_rows == clone_rows and len(source_rows) == len(sample),
            {
                "sample_size": len(sample),
                "source_rows": len(source_rows),
                "clone_rows": len(clone_rows),
            },
        )
        fingerprint = hashlib.sha256(
            ("\n".join(f"{name} {oid}" for name, oid in sorted(refs.items()))
             + "\n"
             + "\n".join(source_rows)).encode()
        ).hexdigest()
        sample_path = self.artifacts / "object-sample.txt"
        sample_path.write_text("\n".join(sample) + "\n", encoding="utf-8")
        self.report["artifacts"]["object_sample"] = str(sample_path)
        self.report["correctness"] = {
            "advertised_refs": refs,
            "clone_refs": clone_refs,
            "source_head": source_head,
            "full_clone_head": full_tip,
            "incremental_clone_head": incremental_tip,
            "sample_size": len(sample),
            "fingerprint": fingerprint,
            "full_fsck": True,
            "incremental_fsck": True,
        }
        self.write_report()

    def summarize_metrics(self) -> None:
        push_durations = [int(push["duration_ms"]) for push in self.report["pushes"]]
        operations: dict[str, list[int]] = {"push": push_durations}
        for name, stage in self.report["stages"].items():
            duration = stage.get("duration_ms")
            if isinstance(duration, int):
                family = "clone" if "clone" in name else "fetch" if "fetch" in name else name
                operations.setdefault(family, []).append(duration)
        summaries = {
            name: {
                "count": len(values),
                "min_ms": min(values) if values else 0,
                "median_ms": percentile(values, 0.50),
                "p95_ms": percentile(values, 0.95),
                "p99_ms": percentile(values, 0.99),
                "max_ms": max(values) if values else 0,
            }
            for name, values in operations.items()
        }
        self.report["metrics"] = {
            "operation_summaries": summaries,
            "replay_pushes": len(push_durations) - 1,
            "total_pushes": len(push_durations),
        }
        self.write_report()

    def cleanup_remote(self) -> None:
        if not self.args.cleanup_remote:
            return
        if not self.remote_prefix.startswith(f"{REMOTE_ROOT}/{self.run_id}"):
            raise QualificationError("refusing cleanup outside this run's remote prefix")
        objects = self.list_remote_objects()
        keys = [str(item["Key"]) for item in objects]
        for offset in range(0, len(keys), 1_000):
            batch = keys[offset : offset + 1_000]
            payload = self.temp_root / f"delete-{offset // 1_000:05d}.json"
            payload.write_text(
                json.dumps({"Objects": [{"Key": key} for key in batch], "Quiet": True}),
                encoding="utf-8",
            )
            self.run_cmd(
                f"delete qualification objects {offset // 1_000 + 1}",
                [
                    str(self.aws_bin),
                    "--endpoint-url",
                    self.args.endpoint_url,
                    "s3api",
                    "delete-objects",
                    "--bucket",
                    self.args.bucket,
                    "--delete",
                    f"file://{payload}",
                ],
                self.run_root,
            )
        remaining = self.list_remote_objects(limit=1)
        self.check(
            "remote-prefix-cleanup",
            not remaining,
            {"prefix": self.remote_prefix, "deleted_objects": len(keys)},
        )
        self.report["cleanup"]["remote_completed"] = True
        self.write_report()

    def cleanup_local_worktrees(self) -> None:
        if self.args.retain_worktrees:
            return
        for path in (
            self.replay_repo,
            self.incremental_clone,
            self.clone_root,
            self.fetch_root,
            self.team_root,
            self.cache_root,
            self.temp_root,
        ):
            if path.exists():
                shutil.rmtree(path)
        self.report["cleanup"]["local_worktrees_removed"] = True
        self.write_report()

    def redaction_check(self) -> None:
        leaked: list[str] = []
        for path in (*self.logs.glob("*.log"), *self.artifacts.glob("*.json")):
            if path.name == "report.json":
                continue
            text = path.read_text(encoding="utf-8", errors="replace")
            if any(secret and secret in text for secret in self.secret_values()):
                leaked.append(str(path))
        self.check("retained-artifacts-redacted", not leaked, {"leaked_files": leaked})

    def run(self) -> int:
        self.setup()
        try:
            source_head, base, commits = self.preflight()
            self.ensure_object_store()
            self.setup_replay(base)
            self.replay(base, commits)
            full_clone = self.final_clones()
            self.verify_correctness(source_head, full_clone)
            self.verify_source_unchanged()
            if self.args.team_load:
                self.run_team_load(source_head)
            self.collect_cache_service_stats()
            self.store_snapshot("final")
            self.summarize_metrics()
            self.cleanup_remote()
            self.redaction_check()
            self.cleanup_local_worktrees()
            self.report["status"] = "ok"
            return 0
        except KeyboardInterrupt:
            self.report["status"] = "failed"
            self.report["error"] = "qualification interrupted"
            print("error: qualification interrupted", file=sys.stderr)
            return 130
        except Exception as error:
            self.report["status"] = "failed"
            self.report["error"] = str(error)
            print(f"error: {error}", file=sys.stderr)
            return 1
        finally:
            if (
                self.args.cleanup_remote
                and not self.report["cleanup"]["remote_completed"]
            ):
                try:
                    self.cleanup_remote()
                except Exception as cleanup_error:
                    self.report["cleanup"]["error"] = str(cleanup_error)
                    if self.report["status"] != "failed":
                        self.report["status"] = "failed"
                        self.report["error"] = f"remote cleanup failed: {cleanup_error}"
            self.report["finished_at"] = utc_now()
            self.write_report()
            print(f"Run ID: {self.run_id}")
            print(f"Remote: {self.remote_url}")
            print(f"Report: {self.artifacts / 'report.json'}")


def parse_size(value: str) -> int:
    try:
        result = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("size must be an integer number of bytes") from error
    if result < 0:
        raise argparse.ArgumentTypeError("size must not be negative")
    return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=DEFAULT_SOURCE)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--run-id")
    parser.add_argument("--bucket", default=DEFAULT_BUCKET)
    parser.add_argument("--endpoint-url", default=DEFAULT_ENDPOINT)
    parser.add_argument("--region", default="us-east-1")
    parser.add_argument("--access-key", default=os.environ.get("AWS_ACCESS_KEY_ID", "crab"))
    parser.add_argument("--secret-key", default=os.environ.get("AWS_SECRET_ACCESS_KEY", "crab"))
    parser.add_argument("--session-token", default=os.environ.get("AWS_SESSION_TOKEN"))
    parser.add_argument(
        "--object-store-version",
        default=os.environ.get("RUSTFS_IMAGE", "external-rustfs"),
    )
    parser.add_argument("--crab-bin", default="crab")
    parser.add_argument("--git-bin", default="git")
    parser.add_argument("--aws-bin", default="aws")
    parser.add_argument("--replay-count", type=int, default=DEFAULT_REPLAY_COUNT)
    parser.add_argument("--sample-size", type=int, default=DEFAULT_SAMPLE_SIZE)
    parser.add_argument("--cold-clone-fanout", type=int, default=0)
    parser.add_argument("--warm-clone-fanout", type=int, default=0)
    parser.add_argument("--team-load", action="store_true")
    parser.add_argument("--fetch-fanout", type=int, default=100)
    parser.add_argument("--independent-pushes", type=int, default=20)
    parser.add_argument("--contended-pushes", type=int, default=20)
    parser.add_argument("--minimum-free-bytes", type=parse_size, default=20 * 1024**3)
    parser.add_argument("--timeout", type=int, default=30 * 60)
    parser.add_argument("--push-timeout", type=int, default=2 * 60 * 60)
    parser.add_argument("--clone-timeout", type=int, default=4 * 60 * 60)
    parser.add_argument("--sample-interval", type=float, default=0.20)
    parser.add_argument("--start-rustfs", action="store_true")
    parser.add_argument("--require-existing-bucket", action="store_true")
    parser.add_argument("--require-cache-service", action="store_true")
    parser.add_argument("--cleanup-remote", action="store_true")
    parser.add_argument("--retain-worktrees", action="store_true")
    args = parser.parse_args()
    if args.replay_count < 1:
        parser.error("--replay-count must be at least 1")
    if args.sample_size < 1:
        parser.error("--sample-size must be at least 1")
    if not 0 <= args.cold_clone_fanout <= 50:
        parser.error("--cold-clone-fanout must be between 0 and 50")
    if not 0 <= args.warm_clone_fanout <= 100:
        parser.error("--warm-clone-fanout must be between 0 and 100")
    if not 1 <= args.fetch_fanout <= 100:
        parser.error("--fetch-fanout must be between 1 and 100")
    if not 1 <= args.independent_pushes <= 20:
        parser.error("--independent-pushes must be between 1 and 20")
    if not 1 <= args.contended_pushes <= 20:
        parser.error("--contended-pushes must be between 1 and 20")
    if args.team_load and args.replay_count < 100:
        parser.error("--team-load requires --replay-count of at least 100")
    if args.sample_interval <= 0:
        parser.error("--sample-interval must be positive")
    return args


def main() -> int:
    try:
        return LargeRepositoryQualification(parse_args()).run()
    except QualificationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
