#!/usr/bin/env python3
"""Run Crab large-file E2E performance and process-chaos workflows.

The script is manual and opt-in. It creates a unique Crab remote under
`crab://<bucket>/e2e-large/<run-id>` and keeps all local repos under
`/Volumes/Workspace/CrabRepos/<run-id>` by default.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import large_file_fixture as fixture


SCRIPT_DIR = Path(__file__).resolve().parent
CRAB_DIR = SCRIPT_DIR.parents[1]
REPO_ROOT = SCRIPT_DIR.parents[2]
DEFAULT_ROOT = Path("/Volumes/Workspace/CrabRepos")
DEFAULT_BUCKET = "crab"
DEFAULT_ENDPOINT = "http://localhost:9000"
REMOTE_PREFIX = "e2e-large"
RUN_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
DENSE_PROFILE = "smoke"
DENSE_ADD_MAX_MS = 50_000
DENSE_GLOBAL_DEDUP_MAX_MS = 40_000
DENSE_REPEATED_DEDUP_MAX_MS = 5_000
DENSE_COLD_HYDRATE_MAX_MS = 90_000
DENSE_WARM_HYDRATE_MAX_MS = 60_000
DENSE_MAX_RSS_BYTES = 512 * fixture.MIB
TIME_RESOURCE_FIELDS = {
    "maximum resident set size": "max_resident_set_size",
}
SECRET_KEYS = {
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_ACCESS_KEY_ID",
}


class WorkflowError(RuntimeError):
    """Raised when a workflow step fails."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def make_run_id() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def slug(value: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9_.-]+", "-", value.strip().lower())
    return cleaned.strip("-") or "command"


def redact_env(env: dict[str, str]) -> dict[str, str]:
    redacted = {}
    for key, value in sorted(env.items()):
        if key in SECRET_KEYS:
            redacted[key] = "<redacted>"
        elif key.startswith("AWS_") or key.startswith("CRAB_"):
            redacted[key] = value
    return redacted


def positive_int(value: str) -> int:
    try:
        parsed = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"{value!r} is not an integer") from exc
    if parsed <= 0:
        raise argparse.ArgumentTypeError(f"{value!r} must be positive")
    return parsed


def ratio(value: str) -> float:
    try:
        parsed = float(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"{value!r} is not a number") from exc
    if not 0.0 <= parsed <= 1.0:
        raise argparse.ArgumentTypeError(f"{value!r} must be between 0 and 1")
    return parsed


def parse_size_mib_list(value: str) -> list[int]:
    sizes: list[int] = []
    for raw_part in value.split(","):
        part = raw_part.strip().lower()
        if not part:
            continue
        multiplier = 1
        for suffix, suffix_multiplier in (
            ("gib", 1024),
            ("gb", 1024),
            ("g", 1024),
            ("mib", 1),
            ("mb", 1),
            ("m", 1),
        ):
            if part.endswith(suffix):
                multiplier = suffix_multiplier
                part = part[: -len(suffix)].strip()
                break
        try:
            size = int(part) * multiplier
        except ValueError as exc:
            raise argparse.ArgumentTypeError(
                f"{raw_part!r} is not a valid MiB/GiB size"
            ) from exc
        if size <= 0:
            raise argparse.ArgumentTypeError(f"{raw_part!r} must be positive")
        sizes.append(size)
    if not sizes:
        raise argparse.ArgumentTypeError("at least one file size is required")
    return sizes


def parse_time_resource_log(path: Path) -> dict[str, int]:
    if not path.exists():
        return {}
    text = path.read_text(errors="replace")
    metrics: dict[str, int] = {}
    for line in text.splitlines():
        stripped = line.strip()
        for suffix, key in TIME_RESOURCE_FIELDS.items():
            if not stripped.endswith(suffix):
                continue
            value = stripped[: -len(suffix)].strip()
            if value:
                try:
                    metrics[key] = int(value)
                except ValueError:
                    pass
    return metrics


def safe_run_id(run_id: str) -> str:
    if not RUN_ID_RE.match(run_id):
        raise WorkflowError(
            "run id must contain only letters, numbers, dot, underscore, or dash"
        )
    if "/" in run_id or "\\" in run_id:
        raise WorkflowError("run id must not contain path separators")
    return run_id


def copy_fixture_file(source: Path, target: Path) -> str:
    clone_command: list[str] | None = None
    if sys.platform == "darwin":
        clone_command = ["cp", "-c", "-p", str(source), str(target)]
    elif sys.platform.startswith("linux"):
        clone_command = [
            "cp",
            "--reflink=always",
            "--sparse=always",
            "--preserve=mode,timestamps",
            str(source),
            str(target),
        ]

    if clone_command is not None:
        result = subprocess.run(
            clone_command,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if result.returncode == 0:
            return "cloned"

    shutil.copy2(source, target)
    return "copied"


@dataclass
class CommandRecord:
    name: str
    args: list[str]
    cwd: str
    stdout_log: str
    stderr_log: str
    started_at: str
    duration_ms: int
    exit_code: int | None
    resource_usage: dict[str, int] | None = None
    expected_failure: bool = False
    killed: bool = False
    ok: bool = True


def evaluate_command_budget(
    record: CommandRecord,
    *,
    max_duration_ms: int,
    max_rss_bytes: int | None = None,
) -> tuple[bool, dict[str, Any]]:
    rss_bytes = (record.resource_usage or {}).get("max_resident_set_size")
    duration_ok = record.duration_ms <= max_duration_ms
    rss_ok = max_rss_bytes is None or (
        rss_bytes is not None and rss_bytes <= max_rss_bytes
    )
    detail = {
        "duration_ms": record.duration_ms,
        "max_duration_ms": max_duration_ms,
        "rss_bytes": rss_bytes,
        "max_rss_bytes": max_rss_bytes,
        "stdout_log": record.stdout_log,
        "stderr_log": record.stderr_log,
    }
    return duration_ok and rss_ok, detail


def evaluate_phase_budget(
    events: list[dict[str, Any]],
    *,
    operation: str,
    phase: str,
    max_duration_ms: int,
) -> tuple[bool, dict[str, Any]]:
    matching = [
        event.get("data")
        for event in events
        if isinstance(event.get("data"), dict)
        and event["data"].get("operation") == operation
        and event["data"].get("phase") == phase
    ]
    elapsed_ms = matching[-1].get("elapsed_ms") if matching else None
    ok = isinstance(elapsed_ms, int) and elapsed_ms <= max_duration_ms
    return ok, {
        "operation": operation,
        "phase": phase,
        "elapsed_ms": elapsed_ms,
        "max_duration_ms": max_duration_ms,
        "matching_events": len(matching),
    }


@dataclass
class Report:
    run_id: str
    profile: str
    remote_url: str
    remote: str
    root: str
    endpoint_url: str
    started_at: str = field(default_factory=utc_now)
    finished_at: str | None = None
    status: str = "running"
    env: dict[str, str] = field(default_factory=dict)
    commands: list[dict[str, Any]] = field(default_factory=list)
    checks: list[dict[str, Any]] = field(default_factory=list)
    scenarios: list[dict[str, Any]] = field(default_factory=list)
    artifacts: dict[str, str] = field(default_factory=dict)


class Runner:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.run_id = safe_run_id(args.run_id or make_run_id())
        self.root = args.root
        self.run_root = self.root / self.run_id
        self.repos_dir = self.run_root / "repos"
        self.artifacts_dir = self.run_root / "artifacts"
        self.logs_dir = self.artifacts_dir / "logs"
        self.cache_dir = self.run_root / "cache"
        self.remote_url = f"crab://{args.bucket}/{REMOTE_PREFIX}/{self.run_id}"
        self.chaos = args.chaos if args.chaos is not None else args.profile == "smoke"
        if args.only_dense_performance:
            self.chaos = False
            args.measure_rss = True
        self.require_chaos_kills = self.chaos and args.profile != "tiny"
        self.env = self.build_env()
        self.command_index = 0
        self.report = Report(
            run_id=self.run_id,
            profile=args.profile,
            remote_url=self.remote_url,
            remote=self.remote_url,
            root=str(self.run_root),
            endpoint_url=args.endpoint_url,
            env=redact_env(self.env),
        )
        self.current_manifest: dict[str, Any] | None = None

    def build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "AWS_ACCESS_KEY_ID": "crab",
                "AWS_SECRET_ACCESS_KEY": "crab",
                "AWS_REGION": "us-east-1",
                "AWS_ENDPOINT_URL": self.args.endpoint_url,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "COPYFILE_DISABLE": "1",
                "COPY_EXTENDED_ATTRIBUTES_DISABLE": "1",
                "CRAB_CACHE_DIR": str(self.cache_dir),
                "GIT_MERGE_AUTOEDIT": "no",
                "GIT_TERMINAL_PROMPT": "0",
            }
        )
        return env

    def scrub_macos_sidecars(self, root: Path) -> None:
        if not root.exists():
            return
        try:
            root.resolve().relative_to(self.run_root.resolve())
        except ValueError:
            return

        for path in sorted(root.rglob("*"), reverse=True):
            if path.name != ".DS_Store" and not path.name.startswith("._"):
                continue
            try:
                if path.is_dir() and not path.is_symlink():
                    shutil.rmtree(path)
                else:
                    path.unlink(missing_ok=True)
            except FileNotFoundError:
                continue

    def count_macos_sidecars(self) -> int:
        if not self.run_root.exists():
            return 0
        return sum(
            1
            for path in self.run_root.rglob("*")
            if path.name == ".DS_Store" or path.name.startswith("._")
        )

    def write_report(self) -> None:
        self.artifacts_dir.mkdir(parents=True, exist_ok=True)
        path = self.artifacts_dir / "report.json"
        payload = self.report.__dict__.copy()
        path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
        self.report.artifacts["report"] = str(path)

    def setup_dirs(self) -> None:
        if self.run_root.exists():
            raise WorkflowError(f"run root already exists: {self.run_root}")
        self.repos_dir.mkdir(parents=True, exist_ok=False)
        self.logs_dir.mkdir(parents=True, exist_ok=True)
        self.report.artifacts["logs"] = str(self.logs_dir)

    def preflight(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        total = self.planned_input_bytes()
        required = total * 5 + 20 * fixture.GIB
        free = shutil.disk_usage(self.root).free
        self.add_check(
            "disk-free",
            free >= required,
            {
                "free_bytes": free,
                "required_bytes": required,
                "profile_bytes": total,
            },
        )
        if free < required:
            raise WorkflowError(
                f"not enough free space under {self.root}: "
                f"need {required} bytes, have {free}"
            )

        try:
            with urllib.request.urlopen(self.args.endpoint_url, timeout=3) as response:
                reachable = response.status < 500
                detail = {"status": response.status}
        except urllib.error.HTTPError as exc:
            reachable = exc.code < 500
            detail = {"status": exc.code}
        except OSError as exc:
            reachable = False
            detail = {"error": str(exc)}
        self.add_check("rustfs-endpoint-reachable", reachable, detail)
        if not reachable:
            raise WorkflowError(
                f"RustFS endpoint is not reachable: {self.args.endpoint_url}"
            )

    def planned_input_bytes(self) -> int:
        if self.args.only_small_edit_push:
            return self.args.small_edit_size_mib * fixture.MIB
        if self.args.only_multi_file_edit_push:
            return sum(parse_size_mib_list(self.args.multi_file_sizes))
        return fixture.profile_bytes(self.args.profile)

    def add_check(self, name: str, ok: bool, detail: dict[str, Any]) -> None:
        self.report.checks.append(
            {
                "name": name,
                "ok": ok,
                "detail": detail,
                "checked_at": utc_now(),
            }
        )

    def add_scenario(self, name: str, status: str, detail: dict[str, Any]) -> None:
        self.report.scenarios.append(
            {
                "name": name,
                "status": status,
                "detail": detail,
                "finished_at": utc_now(),
            }
        )

    def run_cmd(
        self,
        name: str,
        args: list[str],
        *,
        cwd: Path,
        check: bool = True,
        expected_failure: bool = False,
        kill_after: float | None = None,
        timeout: float | None = None,
        extra_env: dict[str, str] | None = None,
        measure_rss: bool = False,
    ) -> CommandRecord:
        self.command_index += 1
        base = f"{self.command_index:03}-{slug(name)}"
        stdout_log = self.logs_dir / f"{base}.stdout.log"
        stderr_log = self.logs_dir / f"{base}.stderr.log"
        started = time.perf_counter()
        started_at = utc_now()
        killed = False
        exit_code: int | None = None

        env = self.env.copy()
        if extra_env:
            env.update(extra_env)

        actual_args = args
        if measure_rss:
            time_bin = Path("/usr/bin/time")
            if not time_bin.exists():
                raise WorkflowError("/usr/bin/time is required for RSS measurement")
            actual_args = [str(time_bin), "-l", *args]

        self.scrub_macos_sidecars(cwd)
        self.scrub_macos_sidecars(self.logs_dir)
        with stdout_log.open("wb") as stdout_fh, stderr_log.open("wb") as stderr_fh:
            proc = subprocess.Popen(
                actual_args,
                cwd=cwd,
                env=env,
                stdout=stdout_fh,
                stderr=stderr_fh,
            )
            try:
                if kill_after is not None:
                    try:
                        exit_code = proc.wait(timeout=kill_after)
                    except subprocess.TimeoutExpired:
                        killed = True
                        proc.terminate()
                        try:
                            exit_code = proc.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            proc.kill()
                            exit_code = proc.wait(timeout=10)
                else:
                    exit_code = proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                killed = True
                proc.kill()
                exit_code = proc.wait(timeout=10)
        self.scrub_macos_sidecars(cwd)

        duration_ms = int((time.perf_counter() - started) * 1000)
        resource_usage = (
            parse_time_resource_log(stderr_log) if measure_rss else None
        )
        failed = exit_code != 0
        ok = failed if expected_failure else not failed
        record = CommandRecord(
            name=name,
            args=args,
            cwd=str(cwd),
            stdout_log=str(stdout_log),
            stderr_log=str(stderr_log),
            started_at=started_at,
            duration_ms=duration_ms,
            exit_code=exit_code,
            resource_usage=resource_usage,
            expected_failure=expected_failure,
            killed=killed,
            ok=ok,
        )
        self.report.commands.append(record.__dict__)
        if kill_after is not None:
            self.record_chaos_kill(record)
        self.write_report()

        if check and not ok:
            expectation = "expected failure" if expected_failure else "success"
            raise WorkflowError(
                f"{name} did not meet expectation ({expectation}); "
                f"exit={exit_code}; stderr={stderr_log}"
            )
        return record

    def record_chaos_kill(self, record: CommandRecord) -> None:
        check_name = f"{slug(record.name)}-process-killed"
        self.add_check(
            check_name,
            record.killed,
            {
                "command": record.name,
                "required": self.require_chaos_kills,
                "exit_code": record.exit_code,
                "stdout_log": record.stdout_log,
                "stderr_log": record.stderr_log,
            },
        )
        if self.require_chaos_kills and not record.killed:
            raise WorkflowError(
                f"{record.name} finished before chaos could kill it; "
                "increase file size/profile or lower --chaos-kill-after"
            )

    def run_git(self, repo: Path, args: list[str], name: str | None = None) -> CommandRecord:
        return self.run_cmd(name or f"git {' '.join(args[:2])}", ["git", *args], cwd=repo)

    def stdout_text(self, record: CommandRecord) -> str:
        return Path(record.stdout_log).read_text(errors="replace")

    def json_lines(self, record: CommandRecord) -> list[dict[str, Any]]:
        records = []
        for line in self.stdout_text(record).splitlines():
            line = line.strip()
            if not line:
                continue
            records.append(json.loads(line))
        return records

    def json_events(self, record: CommandRecord) -> list[dict[str, Any]]:
        records = []
        for log in (record.stdout_log, record.stderr_log):
            for line in Path(log).read_text(errors="replace").splitlines():
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(event, dict):
                    records.append(event)
        return records

    def json_stdout(self, record: CommandRecord) -> Any:
        text = self.stdout_text(record).strip()
        if not text:
            raise WorkflowError(f"{record.name} produced empty stdout")
        return json.loads(text)

    def jsonl_result_data(self, record: CommandRecord) -> dict[str, Any]:
        for event in reversed(self.json_lines(record)):
            event_type = event.get("type", event.get("event_type"))
            if event_type == "result" and isinstance(event.get("data"), dict):
                return event["data"]
        raise WorkflowError(f"{record.name} did not emit a JSONL result")

    def run_crab(
        self,
        repo: Path,
        args: list[str],
        *,
        name: str | None = None,
        check: bool = True,
        expected_failure: bool = False,
        kill_after: float | None = None,
        extra_env: dict[str, str] | None = None,
        measure_rss: bool = False,
    ) -> CommandRecord:
        return self.run_cmd(
            name or f"crab {' '.join(args[:2])}",
            ["crab", *args],
            cwd=repo,
            check=check,
            expected_failure=expected_failure,
            kill_after=kill_after,
            extra_env=extra_env,
            measure_rss=measure_rss,
        )

    def install_crab(self) -> None:
        if self.args.skip_install:
            self.add_scenario("make-install", "skipped", {"skip_install": True})
            return
        self.run_cmd(
            "make install",
            ["make", "-C", str(CRAB_DIR), "install"],
            cwd=REPO_ROOT,
            timeout=60 * 60,
        )
        self.add_scenario("make-install", "ok", {"crab_dir": str(CRAB_DIR)})

    def configure_git_identity(self, repo: Path, who: str) -> None:
        self.run_git(repo, ["config", "user.email", f"{who}@crab-e2e.local"])
        self.run_git(repo, ["config", "user.name", f"Crab E2E {who}"])
        self.run_git(repo, ["config", "commit.gpgsign", "false"])
        self.run_git(repo, ["config", "pull.rebase", "false"])

    def configure_crab_repo(
        self, repo: Path, *, extra_env: dict[str, str] | None = None
    ) -> None:
        self.run_crab(
            repo,
            ["config", "set", "push.lock_ttl_secs", "15"],
            extra_env=extra_env,
        )
        self.run_crab(
            repo,
            ["config", "set", "push.lock_heartbeat_interval", "5"],
            extra_env=extra_env,
        )

    def init_source_repo(self, repo: Path) -> None:
        repo.mkdir(parents=True)
        self.run_git(repo, ["init", "-b", "main"])
        self.configure_git_identity(repo, "source")
        (repo / ".gitignore").write_text("._*\n**/._*\n.DS_Store\n", encoding="utf-8")
        self.run_crab(repo, ["init", self.remote_url], name="crab init source")
        self.configure_crab_repo(repo)
        self.run_crab(repo, ["track", "*.bin"], name="crab track bin")

    def configure_clone(
        self, repo: Path, who: str, *, extra_env: dict[str, str] | None = None
    ) -> None:
        self.configure_git_identity(repo, who)
        self.configure_crab_repo(repo, extra_env=extra_env)

    def crab_add_paths(
        self,
        repo: Path,
        *,
        paths: list[str] | None = None,
        chaos: bool = False,
        extra_env: dict[str, str] | None = None,
        measure_rss: bool = False,
    ) -> CommandRecord:
        existing = [
            path
            for path in (paths or ["*.bin"])
            if path == "*.bin" or (repo / path).exists()
        ]
        if not existing:
            raise WorkflowError("crab add has no existing paths")
        add_args = [
            "add",
            *existing,
            "--jobs",
            str(self.args.jobs),
            "--jsonl",
        ]
        if chaos:
            self.run_crab(
                repo,
                add_args,
                name="chaos kill crab add",
                check=False,
                kill_after=self.args.chaos_kill_after,
                extra_env=extra_env,
            )
        record = self.run_crab(
            repo,
            add_args,
            name="crab add bin",
            extra_env=extra_env,
            measure_rss=measure_rss,
        )
        self.run_git(
            repo,
            ["add", "--", ".crab.toml", ".gitattributes", ".gitignore"],
            name="git add crab metadata",
        )
        git_paths = paths or ["*.bin"]
        self.run_git(
            repo,
            ["add", "-A", "--", *git_paths],
            name="git add bin changes",
        )
        return record

    def commit(self, repo: Path, message: str) -> None:
        self.run_git(repo, ["commit", "-m", message], name=f"git commit {message}")

    def push(
        self,
        repo: Path,
        *,
        chaos: bool = False,
        expected_failure: bool = False,
        extra_env: dict[str, str] | None = None,
        measure_rss: bool = False,
    ) -> CommandRecord:
        push_args = [
            "push",
            "--jsonl",
            "--upload-concurrency",
            str(self.args.upload_concurrency),
        ]
        if expected_failure:
            return self.run_crab(
                repo,
                push_args,
                name="expected stale crab push failure",
                expected_failure=True,
                extra_env=extra_env,
                measure_rss=measure_rss,
            )
        if chaos:
            self.run_crab(
                repo,
                push_args,
                name="chaos kill crab push",
                check=False,
                kill_after=self.args.chaos_kill_after,
                extra_env=extra_env,
            )
        return self.retry_push(repo, push_args, extra_env=extra_env, measure_rss=measure_rss)

    def retry_push(
        self,
        repo: Path,
        push_args: list[str],
        *,
        extra_env: dict[str, str] | None = None,
        measure_rss: bool = False,
    ) -> CommandRecord:
        last: CommandRecord | None = None
        for attempt in range(1, self.args.push_retries + 1):
            last = self.run_crab(
                repo,
                push_args,
                name=f"crab push attempt {attempt}",
                check=False,
                extra_env=extra_env,
                measure_rss=measure_rss,
            )
            if last.exit_code == 0:
                return last
            time.sleep(min(30, attempt * 5))
        raise WorkflowError(
            f"crab push failed after {self.args.push_retries} attempts; "
            f"last stderr={last.stderr_log if last else '<none>'}"
        )

    def clone_repo(
        self,
        target_name: str,
        *,
        eager: bool = False,
        jsonl: bool = True,
        extra_env: dict[str, str] | None = None,
        remote_url: str | None = None,
    ) -> Path:
        target = self.repos_dir / target_name
        args = ["clone", remote_url or self.remote_url, str(target)]
        if eager:
            args.append("--eager")
        if jsonl:
            args.append("--jsonl")
        self.run_cmd(
            f"crab clone {target_name}",
            ["crab", *args],
            cwd=self.repos_dir,
            timeout=60 * 60,
            extra_env=extra_env,
        )
        self.configure_clone(target, target_name, extra_env=extra_env)
        return target

    def mirror_manifest_files(
        self, source: Path, target: Path, manifest: dict[str, Any]
    ) -> dict[str, int]:
        stats = {"cloned": 0, "copied": 0, "skipped_deleted": 0}
        for entry in manifest["files"]:
            relative = entry["path"]
            if entry.get("deleted"):
                stats["skipped_deleted"] += 1
                continue
            src = source / relative
            dst = target / relative
            dst.parent.mkdir(parents=True, exist_ok=True)
            strategy = copy_fixture_file(src, dst)
            stats[strategy] += 1
        return stats

    def push_xorb_upload_metrics(self, record: CommandRecord) -> dict[str, int]:
        metrics = {"item_count": 0, "bytes_out": 0, "events": 0}
        for event in self.json_lines(record):
            data = event.get("data") or {}
            if event.get("schema") == "perf.phase" and data.get("phase") == "xorb_upload":
                metrics["events"] += 1
                metrics["item_count"] += int(data.get("item_count") or 0)
                metrics["bytes_out"] += int(data.get("bytes_out") or 0)
        return metrics

    def crab_add_single_path(
        self,
        repo: Path,
        path: str,
        *,
        name: str,
        measure_rss: bool = False,
    ) -> CommandRecord:
        return self.crab_add_path_set(
            repo,
            [path],
            name=name,
            measure_rss=measure_rss,
        )

    def crab_add_path_set(
        self,
        repo: Path,
        paths: list[str],
        *,
        name: str,
        measure_rss: bool = False,
    ) -> CommandRecord:
        if not paths:
            raise WorkflowError(f"{name} has no paths to add")
        record = self.run_crab(
            repo,
            ["add", *paths, "--jobs", str(self.args.jobs), "--jsonl"],
            name=name,
            measure_rss=measure_rss,
        )
        self.run_git(
            repo,
            ["add", "--", ".crab.toml", ".gitattributes", ".gitignore"],
            name=f"{name} git add crab metadata",
        )
        self.run_git(
            repo,
            ["add", "-A", "--", *paths],
            name=f"{name} git add pointer",
        )
        return record

    def push_plan_stats(self, repo: Path, *, name: str) -> tuple[CommandRecord, dict[str, Any]]:
        record = self.run_crab(
            repo,
            ["stat", "push-plan", "--verify", "--json"],
            name=name,
        )
        payload = self.json_stdout(record)
        data = payload.get("data") if isinstance(payload, dict) else None
        stats = data.get("stats", data) if isinstance(data, dict) else None
        if not isinstance(stats, dict):
            raise WorkflowError(f"{name} did not return push-plan stats")
        return record, stats

    def hydrate_all(
        self,
        repo: Path,
        *,
        chaos: bool = False,
        extra_env: dict[str, str] | None = None,
        measure_rss: bool = False,
        name: str = "crab hydrate all",
    ) -> CommandRecord:
        args = ["hydrate", "--all", "--jsonl"]
        if chaos:
            self.run_crab(
                repo,
                args,
                name="chaos kill crab hydrate",
                check=False,
                kill_after=self.args.chaos_kill_after,
                extra_env=extra_env,
            )
        return self.run_crab(
            repo,
            args,
            name=name,
            extra_env=extra_env,
            measure_rss=measure_rss,
        )

    def pull(self, repo: Path, *, expected_failure: bool = False) -> CommandRecord:
        self.run_crab(
            repo,
            ["dehydrate", "--all", "--jsonl"],
            name="crab dehydrate before pull",
        )
        self.run_git(repo, ["add", "-u", "--", "*.bin"], name="git refresh bin index")
        return self.run_crab(
            repo,
            ["pull", "--remote", "origin", "--branch", "main", "--no-hydrate", "--jsonl"],
            name="crab pull",
            expected_failure=expected_failure,
        )

    def verify(self, repo: Path, manifest: dict[str, Any], name: str) -> None:
        results = fixture.verify_manifest(repo, manifest)
        failures = [result for result in results if not result["ok"]]
        self.add_check(
            name,
            not failures,
            {"repo": str(repo), "files": len(results), "failures": failures[:5]},
        )
        if failures:
            raise WorkflowError(f"{name} failed; first failures: {failures[:3]}")

    def mutation(
        self,
        repo: Path,
        base_manifest: dict[str, Any],
        name: str,
        artifact_name: str,
    ) -> dict[str, Any]:
        path = self.artifacts_dir / f"{artifact_name}.manifest.json"
        return fixture.apply_mutation(repo, base_manifest, name, path)

    def scan_current(self, repo: Path, artifact_name: str) -> dict[str, Any]:
        path = self.artifacts_dir / f"{artifact_name}.manifest.json"
        return fixture.scan_manifest(repo, self.args.profile, path)

    def merge_non_overlap_manifest(
        self,
        base_manifest: dict[str, Any],
        overlay_manifest: dict[str, Any],
        artifact_name: str,
    ) -> dict[str, Any]:
        entries = fixture.manifest_entries(base_manifest)
        overlay_entries = fixture.manifest_entries(overlay_manifest)
        missing = []
        for path in overlay_manifest.get("changed_paths", []):
            overlay = overlay_entries.get(path)
            if overlay is None:
                missing.append(path)
                continue
            entries[path] = overlay
        if missing:
            raise WorkflowError(
                f"overlay manifest is missing changed path(s): {', '.join(missing)}"
            )

        merged = fixture.build_manifest(
            profile=base_manifest.get("profile", self.args.profile),
            root=self.run_root,
            files=entries.values(),
            mutation=f"merge:{artifact_name}",
            changed_paths=overlay_manifest.get("changed_paths", []),
        )
        path = self.artifacts_dir / f"{artifact_name}.manifest.json"
        fixture.write_manifest(path, merged)
        return merged

    def fresh_cache_env(self, name: str) -> dict[str, str]:
        cache_dir = self.run_root / "fresh-caches" / name
        cache_dir.mkdir(parents=True, exist_ok=True)
        return {"CRAB_CACHE_DIR": str(cache_dir)}

    def record_command_budget(
        self,
        name: str,
        record: CommandRecord,
        *,
        max_duration_ms: int,
        max_rss_bytes: int | None = None,
    ) -> bool:
        ok, detail = evaluate_command_budget(
            record,
            max_duration_ms=max_duration_ms,
            max_rss_bytes=max_rss_bytes,
        )
        self.add_check(name, ok, detail)
        return ok

    def record_phase_budget(
        self,
        name: str,
        record: CommandRecord,
        *,
        operation: str,
        phase: str,
        max_duration_ms: int,
    ) -> bool:
        ok, detail = evaluate_phase_budget(
            self.json_events(record),
            operation=operation,
            phase=phase,
            max_duration_ms=max_duration_ms,
        )
        detail["stdout_log"] = record.stdout_log
        detail["stderr_log"] = record.stderr_log
        self.add_check(name, ok, detail)
        return ok

    def run_fresh_cache_remote_probe(
        self, manifest: dict[str, Any], target_name: str
    ) -> None:
        env = self.fresh_cache_env(target_name)
        probe = self.clone_repo(target_name, extra_env=env)
        self.hydrate_all(probe, extra_env=env)
        self.verify(probe, manifest, f"{target_name}-hashes")
        record = self.run_crab(
            probe,
            ["fsck"],
            name=f"{target_name} crab fsck",
            check=False,
            extra_env=env,
        )
        ok = record.exit_code == 0
        self.add_check(
            f"{target_name}-fsck",
            ok,
            {
                "repo": str(probe),
                "cache_dir": env["CRAB_CACHE_DIR"],
                "stdout_log": record.stdout_log,
                "stderr_log": record.stderr_log,
                "exit_code": record.exit_code,
            },
        )
        if not ok:
            raise WorkflowError(f"{target_name} fsck failed; stderr={record.stderr_log}")
        self.add_scenario(
            "fresh-cache-remote-proof",
            "ok",
            {"repo": str(probe), "cache_dir": env["CRAB_CACHE_DIR"]},
        )

    def run_initial_push(
        self, source: Path
    ) -> tuple[dict[str, Any], CommandRecord, CommandRecord]:
        manifest_path = self.artifacts_dir / "initial.manifest.json"
        manifest = fixture.create_profile(source, self.args.profile, manifest_path)
        self.report.artifacts["initial_manifest"] = str(manifest_path)
        add_record = self.crab_add_paths(
            source,
            paths=[entry["path"] for entry in manifest["files"]],
            chaos=self.chaos,
            measure_rss=self.args.measure_rss,
        )
        self.commit(source, "e2e initial large files")
        push_record = self.push(
            source,
            chaos=self.chaos,
            measure_rss=self.args.measure_rss,
        )
        self.add_scenario(
            "initial-large-file-push",
            "ok",
            {
                "files": len(manifest["files"]),
                "bytes": manifest["total_bytes"],
                "add_ms": add_record.duration_ms,
                "push_ms": push_record.duration_ms,
                "add_rss": (add_record.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
                "push_rss": (push_record.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
            },
        )
        if self.chaos:
            self.run_fresh_cache_remote_probe(manifest, "fresh-cache-after-chaos-push")
        return manifest, add_record, push_record

    def run_small_edit_push_benchmark(self) -> None:
        source = self.repos_dir / "small-edit-source"
        self.init_source_repo(source)

        relative = "data/model-small-edit.bin"
        file_size = self.args.small_edit_size_mib * fixture.MIB
        edit_bytes = self.args.small_edit_bytes
        if edit_bytes >= file_size:
            raise WorkflowError("small edit byte count must be smaller than the file")

        initial_manifest_path = self.artifacts_dir / "small-edit-initial.manifest.json"
        entry = fixture.write_deterministic_file(
            source,
            relative,
            file_size,
            version=1,
            seed=0x5EED_4517,
        )
        initial_manifest = fixture.build_manifest(
            profile="small-edit-push",
            root=source,
            files=[entry],
        )
        fixture.write_manifest(initial_manifest_path, initial_manifest)
        self.report.artifacts["small_edit_initial_manifest"] = str(initial_manifest_path)

        initial_add = self.crab_add_single_path(
            source,
            relative,
            name="small edit initial crab add",
            measure_rss=self.args.measure_rss,
        )
        self.commit(source, "e2e small edit initial")
        initial_push = self.push(source, measure_rss=self.args.measure_rss)

        updated_manifest_path = self.artifacts_dir / "small-edit-updated.manifest.json"
        updated_entry = fixture.rewrite_small_delta(
            source,
            entry,
            "small-edit-push",
            span=edit_bytes,
        )
        updated_manifest = fixture.build_manifest(
            profile="small-edit-push",
            root=source,
            files=[updated_entry],
            mutation="small-edit",
            changed_paths=[relative],
        )
        fixture.write_manifest(updated_manifest_path, updated_manifest)
        self.report.artifacts["small_edit_updated_manifest"] = str(updated_manifest_path)

        second_add = self.crab_add_single_path(
            source,
            relative,
            name="small edit second crab add",
            measure_rss=self.args.measure_rss,
        )
        plan_record, plan_stats = self.push_plan_stats(
            source,
            name="small edit push plan verify",
        )
        planned_chunks = int(plan_stats.get("planned_chunks") or 0)
        covered_chunks = int(plan_stats.get("existing_chunks") or 0) + int(
            plan_stats.get("prepared_chunks") or 0
        )
        cover_ratio = (covered_chunks / planned_chunks) if planned_chunks else 0.0
        plan_ok = (
            int(plan_stats.get("plan_files") or 0) == 1
            and int(plan_stats.get("invalid_plan_files") or 0) == 0
            and cover_ratio >= self.args.small_edit_min_plan_cover_ratio
            and int(plan_stats.get("missing_prepared_xorb_files") or 0) == 0
            and int(plan_stats.get("mismatched_prepared_xorb_files") or 0) == 0
            and int(plan_stats.get("payload_hash_mismatched_prepared_xorb_files") or 0) == 0
            and int(plan_stats.get("corrupt_prepared_xorb_files") or 0) == 0
            and int(plan_stats.get("metadata_mismatched_prepared_xorb_files") or 0) == 0
        )
        self.add_check(
            "small-edit-push-plan-covers-unchanged-chunks",
            plan_ok,
            {
                "planned_chunks": planned_chunks,
                "covered_chunks": covered_chunks,
                "cover_ratio": cover_ratio,
                "min_cover_ratio": self.args.small_edit_min_plan_cover_ratio,
                "stats": plan_stats,
                "stdout_log": plan_record.stdout_log,
                "stderr_log": plan_record.stderr_log,
            },
        )
        if not plan_ok:
            raise WorkflowError("small edit push plan did not cover enough unchanged chunks")

        self.commit(source, "e2e small edit delta")
        second_push = self.push(source, measure_rss=self.args.measure_rss)
        xorb_upload = self.push_xorb_upload_metrics(second_push)
        upload_budget = self.args.small_edit_upload_budget_mib * fixture.MIB
        duration_budget_ms = self.args.small_edit_push_budget_secs * 1000
        upload_ok = (
            xorb_upload["events"] > 0
            and xorb_upload["bytes_out"] <= upload_budget
            and xorb_upload["bytes_out"] < file_size // 10
            and second_push.duration_ms <= duration_budget_ms
        )
        self.add_check(
            "small-edit-second-push-delta-upload",
            upload_ok,
            {
                "file_size": file_size,
                "edit_bytes": edit_bytes,
                "upload_budget": upload_budget,
                "duration_budget_ms": duration_budget_ms,
                "second_push_duration_ms": second_push.duration_ms,
                "xorb_upload": xorb_upload,
                "stdout_log": second_push.stdout_log,
                "stderr_log": second_push.stderr_log,
            },
        )
        if not upload_ok:
            raise WorkflowError("small edit second push exceeded delta upload or time budget")

        if self.args.measure_rss:
            rss_budget = self.args.small_edit_max_rss_gib * fixture.GIB
            rss_values = {
                "initial_add": (initial_add.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
                "initial_push": (initial_push.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
                "second_add": (second_add.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
                "second_push": (second_push.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
            }
            rss_ok = all(value is not None and value <= rss_budget for value in rss_values.values())
            self.add_check(
                "small-edit-rss-budget",
                rss_ok,
                {
                    "rss_budget_bytes": rss_budget,
                    "rss": rss_values,
                },
            )
            if not rss_ok:
                raise WorkflowError("small edit command RSS exceeded budget")

        clone = self.clone_repo("small-edit-clone")
        self.hydrate_all(clone)
        self.verify(clone, updated_manifest, "small-edit-clone-hashes")
        self.add_scenario(
            "small-edit-push-benchmark",
            "ok",
            {
                "file_size": file_size,
                "edit_bytes": edit_bytes,
                "initial_add_ms": initial_add.duration_ms,
                "initial_push_ms": initial_push.duration_ms,
                "second_add_ms": second_add.duration_ms,
                "second_push_ms": second_push.duration_ms,
                "xorb_upload": xorb_upload,
                "plan_stats": plan_stats,
            },
        )

    def run_multi_file_edit_push_benchmark(self) -> None:
        source = self.repos_dir / "multi-file-source"
        self.init_source_repo(source)

        sizes_mib = parse_size_mib_list(self.args.multi_file_sizes)
        edit_bytes = self.args.multi_edit_bytes
        paths = [
            f"data/multi-model-{idx:03}-{size_mib}m.bin"
            for idx, size_mib in enumerate(sizes_mib)
        ]
        entries = []
        seed_offset = self.args.multi_seed_offset
        for idx, (relative, size_mib) in enumerate(zip(paths, sizes_mib, strict=True)):
            size = size_mib * fixture.MIB
            if edit_bytes >= size:
                raise WorkflowError(
                    f"multi edit byte count must be smaller than {relative}"
                )
            entries.append(
                fixture.write_deterministic_file(
                    source,
                    relative,
                    size,
                    version=1,
                    seed=0x6170_0000 + seed_offset + idx,
                )
            )

        initial_manifest_path = self.artifacts_dir / "multi-file-initial.manifest.json"
        manifest = fixture.build_manifest(
            profile="multi-file-edit-push",
            root=source,
            files=entries,
        )
        fixture.write_manifest(initial_manifest_path, manifest)
        self.report.artifacts["multi_file_initial_manifest"] = str(initial_manifest_path)

        initial_add = self.crab_add_path_set(
            source,
            paths,
            name="multi file initial crab add",
            measure_rss=self.args.measure_rss,
        )
        self.commit(source, "e2e multi file initial")
        initial_push = self.push(source, measure_rss=self.args.measure_rss)
        initial_xorb_upload = self.push_xorb_upload_metrics(initial_push)

        round_reports: list[dict[str, Any]] = []
        current_entries = entries
        total_bytes = sum(entry["size"] for entry in current_entries)
        edit_file_count = self.args.multi_edit_file_count or len(current_entries)
        if edit_file_count > len(current_entries):
            raise WorkflowError(
                "multi edit file count must not exceed the number of multi-file inputs"
            )
        for edit_round, label in ((2, "second"), (3, "third")):
            updated_entries = []
            changed_paths = []
            for idx, entry in enumerate(current_entries):
                if idx < edit_file_count:
                    updated = fixture.rewrite_small_delta(
                        source,
                        entry,
                        f"multi-file-{label}-edit:{entry['path']}",
                        span=edit_bytes,
                    )
                    changed_paths.append(updated["path"])
                    updated_entries.append(updated)
                else:
                    updated_entries.append(entry)
            manifest_path = self.artifacts_dir / f"multi-file-{label}-edit.manifest.json"
            manifest = fixture.build_manifest(
                profile="multi-file-edit-push",
                root=source,
                files=updated_entries,
                mutation=f"multi-file-{label}-edit",
                changed_paths=changed_paths,
            )
            fixture.write_manifest(manifest_path, manifest)
            self.report.artifacts[f"multi_file_{label}_edit_manifest"] = str(manifest_path)

            add_paths = paths if self.args.multi_add_all_candidates else changed_paths
            add_record = self.crab_add_path_set(
                source,
                add_paths,
                name=f"multi file {label} edit crab add",
                measure_rss=self.args.measure_rss,
            )
            add_result = self.jsonl_result_data(add_record)
            expected_skipped = len(add_paths) - len(changed_paths)
            add_skip_ok = (
                int(add_result.get("files_staged") or 0) == len(changed_paths)
                and int(add_result.get("files_skipped") or 0) == expected_skipped
                and int(add_result.get("files_failed") or 0) == 0
            )
            self.add_check(
                f"multi-file-{label}-edit-add-skips-clean-candidates",
                add_skip_ok,
                {
                    "changed_files": len(changed_paths),
                    "add_candidate_files": len(add_paths),
                    "expected_skipped": expected_skipped,
                    "add_result": add_result,
                    "stdout_log": add_record.stdout_log,
                    "stderr_log": add_record.stderr_log,
                },
            )
            if not add_skip_ok:
                raise WorkflowError(
                    f"multi-file {label} edit add did not skip the expected clean files"
                )
            plan_record, plan_stats = self.push_plan_stats(
                source,
                name=f"multi file {label} edit push plan verify",
            )
            planned_chunks = int(plan_stats.get("planned_chunks") or 0)
            covered_chunks = int(plan_stats.get("existing_chunks") or 0) + int(
                plan_stats.get("prepared_chunks") or 0
            )
            cover_ratio = (covered_chunks / planned_chunks) if planned_chunks else 0.0
            plan_ok = (
                int(plan_stats.get("plan_files") or 0) == len(changed_paths)
                and int(plan_stats.get("invalid_plan_files") or 0) == 0
                and cover_ratio >= self.args.multi_edit_min_plan_cover_ratio
                and int(plan_stats.get("missing_prepared_xorb_files") or 0) == 0
                and int(plan_stats.get("mismatched_prepared_xorb_files") or 0) == 0
                and int(plan_stats.get("payload_hash_mismatched_prepared_xorb_files") or 0)
                == 0
                and int(plan_stats.get("corrupt_prepared_xorb_files") or 0) == 0
                and int(plan_stats.get("metadata_mismatched_prepared_xorb_files") or 0)
                == 0
            )
            self.add_check(
                f"multi-file-{label}-edit-push-plan-covers-unchanged-chunks",
                plan_ok,
                {
                    "planned_chunks": planned_chunks,
                    "covered_chunks": covered_chunks,
                    "cover_ratio": cover_ratio,
                    "min_cover_ratio": self.args.multi_edit_min_plan_cover_ratio,
                    "stats": plan_stats,
                    "stdout_log": plan_record.stdout_log,
                    "stderr_log": plan_record.stderr_log,
                },
            )
            if not plan_ok:
                raise WorkflowError(
                    f"multi-file {label} edit push plan did not cover enough chunks"
                )

            self.commit(source, f"e2e multi file {label} edit")
            push_record = self.push(source, measure_rss=self.args.measure_rss)
            xorb_upload = self.push_xorb_upload_metrics(push_record)
            upload_budget = self.args.multi_edit_upload_budget_mib * fixture.MIB
            duration_budget_ms = self.args.multi_edit_push_budget_secs * 1000
            upload_ok = (
                xorb_upload["events"] > 0
                and xorb_upload["bytes_out"] <= upload_budget
                and xorb_upload["bytes_out"] < total_bytes // 10
                and push_record.duration_ms <= duration_budget_ms
            )
            self.add_check(
                f"multi-file-{label}-edit-delta-upload",
                upload_ok,
                {
                    "file_count": len(changed_paths),
                    "add_candidate_count": len(add_paths),
                    "total_bytes": total_bytes,
                    "edit_bytes_per_file": edit_bytes,
                    "upload_budget": upload_budget,
                    "duration_budget_ms": duration_budget_ms,
                    "push_duration_ms": push_record.duration_ms,
                    "xorb_upload": xorb_upload,
                    "stdout_log": push_record.stdout_log,
                    "stderr_log": push_record.stderr_log,
                },
            )
            if not upload_ok:
                raise WorkflowError(
                    f"multi-file {label} edit exceeded delta upload or time budget"
                )

            round_reports.append(
                {
                    "round": edit_round,
                    "label": label,
                    "add_ms": add_record.duration_ms,
                    "push_ms": push_record.duration_ms,
                    "changed_files": len(changed_paths),
                    "add_candidate_files": len(add_paths),
                    "add_result": add_result,
                    "add_rss": (add_record.resource_usage or {}).get(
                        "max_resident_set_size"
                    ),
                    "push_rss": (push_record.resource_usage or {}).get(
                        "max_resident_set_size"
                    ),
                    "xorb_upload": xorb_upload,
                    "plan_stats": plan_stats,
                }
            )
            current_entries = updated_entries

        if self.args.measure_rss:
            rss_budget = self.args.multi_edit_max_rss_gib * fixture.GIB
            rss_values = {
                "initial_add": (initial_add.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
                "initial_push": (initial_push.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
            }
            for round_report in round_reports:
                label = round_report["label"]
                rss_values[f"{label}_edit_add"] = round_report["add_rss"]
                rss_values[f"{label}_edit_push"] = round_report["push_rss"]
            rss_ok = all(value is not None and value <= rss_budget for value in rss_values.values())
            self.add_check(
                "multi-file-rss-budget",
                rss_ok,
                {
                    "rss_budget_bytes": rss_budget,
                    "rss": rss_values,
                },
            )
            if not rss_ok:
                raise WorkflowError("multi-file command RSS exceeded budget")

        clone = self.clone_repo("multi-file-clone")
        self.hydrate_all(clone)
        self.verify(clone, manifest, "multi-file-clone-hashes")
        self.add_scenario(
            "multi-file-edit-push-benchmark",
            "ok",
            {
                "file_sizes_mib": sizes_mib,
                "file_count": len(sizes_mib),
                "total_bytes": total_bytes,
                "edit_bytes_per_file": edit_bytes,
                "edit_file_count": edit_file_count,
                "add_all_candidates": self.args.multi_add_all_candidates,
                "seed_offset": seed_offset,
                "initial_add_ms": initial_add.duration_ms,
                "initial_push_ms": initial_push.duration_ms,
                "initial_add_rss": (initial_add.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
                "initial_push_rss": (initial_push.resource_usage or {}).get(
                    "max_resident_set_size"
                ),
                "initial_xorb_upload": initial_xorb_upload,
                "edit_rounds": round_reports,
            },
        )

    def run_dedup_peer(
        self,
        source: Path,
        manifest: dict[str, Any],
        *,
        name: str,
        cache_env: dict[str, str],
        measure_rss: bool = False,
    ) -> dict[str, Any]:
        peer = self.repos_dir / f"{name}-source"
        peer_remote = f"{self.remote_url}/{name}"

        peer.mkdir(parents=True)
        self.run_git(peer, ["init", "-b", "main"])
        self.configure_git_identity(peer, name)
        (peer / ".gitignore").write_text("._*\n**/._*\n.DS_Store\n", encoding="utf-8")
        self.run_crab(
            peer,
            ["init", peer_remote],
            name=f"crab init {name}",
            extra_env=cache_env,
        )
        self.configure_crab_repo(peer, extra_env=cache_env)
        self.run_crab(
            peer,
            ["track", "*.bin"],
            name=f"crab track bin {name}",
            extra_env=cache_env,
        )

        mirror_stats = self.mirror_manifest_files(source, peer, manifest)
        add_record = self.crab_add_paths(
            peer,
            paths=[entry["path"] for entry in manifest["files"]],
            extra_env=cache_env,
            measure_rss=measure_rss,
        )
        self.commit(peer, f"e2e {name}")
        push_record = self.push(
            peer,
            extra_env=cache_env,
            measure_rss=measure_rss,
        )
        xorb_upload = self.push_xorb_upload_metrics(push_record)
        ok = xorb_upload["events"] > 0 and xorb_upload["item_count"] == 0
        ok = ok and xorb_upload["bytes_out"] == 0
        self.add_check(
            f"{name}-no-xorb-upload",
            ok,
            {
                "repo": str(peer),
                "remote": peer_remote,
                "cache_dir": cache_env["CRAB_CACHE_DIR"],
                "xorb_upload": xorb_upload,
                "stdout_log": push_record.stdout_log,
                "stderr_log": push_record.stderr_log,
            },
        )
        if not ok:
            raise WorkflowError(
                f"{name} uploaded xorbs instead of reusing bucket-global chunks"
            )
        return {
            "repo": peer,
            "remote": peer_remote,
            "mirror": mirror_stats,
            "add": add_record,
            "push": push_record,
            "xorb_upload": xorb_upload,
        }

    def run_cross_repo_dedup(self, source: Path, manifest: dict[str, Any]) -> None:
        fresh_env = self.fresh_cache_env("cross-repo-dedup")
        result = self.run_dedup_peer(
            source,
            manifest,
            name="cross-repo-dedup",
            cache_env=fresh_env,
            measure_rss=self.args.measure_rss,
        )

        hydrate_env = self.fresh_cache_env("cross-repo-dedup-hydrate")
        peer_clone = self.clone_repo(
            "cross-repo-dedup-clone",
            extra_env=hydrate_env,
            remote_url=result["remote"],
        )
        self.hydrate_all(
            peer_clone,
            extra_env=hydrate_env,
            measure_rss=self.args.measure_rss,
        )
        self.verify(peer_clone, manifest, "cross-repo-dedup-clone-hashes")
        self.add_scenario(
            "cross-repo-dedup",
            "ok",
            {
                "remote": result["remote"],
                "mirror": result["mirror"],
                "xorb_upload": result["xorb_upload"],
                "cache_dir": fresh_env["CRAB_CACHE_DIR"],
            },
        )

    def run_dense_performance_benchmark(
        self,
        source: Path,
        manifest: dict[str, Any],
        initial_add: CommandRecord,
    ) -> None:
        shared_dedup_env = self.fresh_cache_env("dense-global-dedup")
        cold = self.run_dedup_peer(
            source,
            manifest,
            name="cold-global-dedup",
            cache_env=shared_dedup_env,
            measure_rss=True,
        )
        repeated = self.run_dedup_peer(
            source,
            manifest,
            name="repeated-global-dedup",
            cache_env=shared_dedup_env,
            measure_rss=True,
        )

        hydrate_env = self.fresh_cache_env("dense-hydrate")
        clone = self.clone_repo(
            "dense-hydrate-clone",
            extra_env=hydrate_env,
            remote_url=cold["remote"],
        )
        cold_hydrate = self.hydrate_all(
            clone,
            extra_env=hydrate_env,
            measure_rss=True,
            name="cold dense hydrate",
        )
        self.verify(clone, manifest, "cold-dense-hydrate-hashes")
        self.run_crab(
            clone,
            ["dehydrate", "--all", "--jsonl"],
            name="dehydrate before warm dense hydrate",
            extra_env=hydrate_env,
        )
        warm_hydrate = self.hydrate_all(
            clone,
            extra_env=hydrate_env,
            measure_rss=True,
            name="warm dense hydrate",
        )
        self.verify(clone, manifest, "warm-dense-hydrate-hashes")

        checks = [
            self.record_command_budget(
                "dense-add-performance-budget",
                initial_add,
                max_duration_ms=DENSE_ADD_MAX_MS,
                max_rss_bytes=DENSE_MAX_RSS_BYTES,
            ),
            self.record_phase_budget(
                "cold-global-dedup-performance-budget",
                cold["push"],
                operation="push",
                phase="chunk_index_global_lookup",
                max_duration_ms=DENSE_GLOBAL_DEDUP_MAX_MS,
            ),
            self.record_command_budget(
                "cold-global-dedup-rss-budget",
                cold["push"],
                max_duration_ms=cold["push"].duration_ms,
                max_rss_bytes=DENSE_MAX_RSS_BYTES,
            ),
            self.record_phase_budget(
                "repeated-global-dedup-performance-budget",
                repeated["push"],
                operation="push",
                phase="chunk_index_global_lookup",
                max_duration_ms=DENSE_REPEATED_DEDUP_MAX_MS,
            ),
            self.record_command_budget(
                "cold-hydrate-performance-budget",
                cold_hydrate,
                max_duration_ms=DENSE_COLD_HYDRATE_MAX_MS,
                max_rss_bytes=DENSE_MAX_RSS_BYTES,
            ),
            self.record_command_budget(
                "warm-hydrate-performance-budget",
                warm_hydrate,
                max_duration_ms=DENSE_WARM_HYDRATE_MAX_MS,
                max_rss_bytes=DENSE_MAX_RSS_BYTES,
            ),
        ]
        self.add_scenario(
            "dense-performance-gates",
            "ok" if all(checks) else "failed",
            {
                "initial_add_ms": initial_add.duration_ms,
                "cold_dedup_push_ms": cold["push"].duration_ms,
                "repeated_dedup_push_ms": repeated["push"].duration_ms,
                "cold_hydrate_ms": cold_hydrate.duration_ms,
                "warm_hydrate_ms": warm_hydrate.duration_ms,
            },
        )
        if not all(checks):
            raise WorkflowError("one or more dense performance gates failed")

    def run_clone_checks(self, manifest: dict[str, Any]) -> tuple[Path, Path]:
        lazy = self.clone_repo("lazy")
        first_path = manifest["files"][0]["path"]
        self.run_crab(lazy, ["hydrate", first_path, "--jsonl"], name="crab hydrate selective")
        self.hydrate_all(lazy, chaos=self.chaos)
        self.verify(lazy, manifest, "lazy-clone-hydrate-hashes")

        eager = self.clone_repo("eager", eager=True)
        self.verify(eager, manifest, "eager-clone-hashes")
        self.run_crab(eager, ["dehydrate", "--all", "--jsonl"], name="crab dehydrate eager")
        self.run_crab(eager, ["status", "--porcelain"], name="crab status after dehydrate")
        self.hydrate_all(eager)
        self.verify(eager, manifest, "dehydrate-rehydrate-hashes")

        self.add_scenario("clone-hydrate-dehydrate", "ok", {})
        return lazy, eager

    def run_diff_checks(self, repo: Path, changed_paths: list[str]) -> None:
        expected = set(changed_paths)
        json_record = self.run_crab(
            repo,
            ["diff", "--json", "HEAD~1", "HEAD"],
            name="crab diff json delta",
        )
        payload = self.json_stdout(json_record)
        reports = [item["report"] for item in payload["data"]["files"]]
        reported_paths = {report["path"] for report in reports}
        statuses = {report["path"]: report["status"] for report in reports}
        summary = payload["data"]["summary"]
        json_ok = expected.issubset(reported_paths)
        json_ok = json_ok and summary["files_changed"] >= len(expected)
        self.add_check(
            "crab-diff-json-delta",
            json_ok,
            {
                "expected_paths": sorted(expected),
                "reported_paths": sorted(reported_paths),
                "statuses": statuses,
                "summary": summary,
                "stdout_log": json_record.stdout_log,
            },
        )
        if not json_ok:
            raise WorkflowError("crab diff --json did not report all delta paths")

        dedup_reports = [
            report
            for report in reports
            if report["status"] == "modified" and report.get("unchanged_bytes", 0) > 0
        ]
        dedup_ok = any(report.get("dedup_ratio", 0) > 0 for report in dedup_reports)
        dedup_required = self.args.profile != "tiny"
        self.add_check(
            "versioned-large-file-delta-dedup",
            dedup_ok or not dedup_required,
            {
                "required": dedup_required,
                "modified_reports": [
                    {
                        "path": report["path"],
                        "dedup_ratio": report.get("dedup_ratio"),
                        "unchanged_bytes": report.get("unchanged_bytes"),
                        "delta_bytes": report.get("delta_bytes"),
                    }
                    for report in dedup_reports
                ],
                "stdout_log": json_record.stdout_log,
            },
        )
        if dedup_required and not dedup_ok:
            raise WorkflowError("versioned large-file diff did not prove delta dedup")

        stat_record = self.run_crab(
            repo,
            ["diff", "--stat", "HEAD~1", "HEAD"],
            name="crab diff stat delta",
        )
        stat_text = self.stdout_text(stat_record)
        stat_ok = "files changed" in stat_text and "chunks changed" in stat_text
        self.add_check(
            "crab-diff-stat-delta",
            stat_ok,
            {"stdout": stat_text.strip(), "stdout_log": stat_record.stdout_log},
        )
        if not stat_ok:
            raise WorkflowError("crab diff --stat output was not recognized")

        name_record = self.run_crab(
            repo,
            ["diff", "--name-only", "HEAD~1", "HEAD"],
            name="crab diff name-only delta",
        )
        names = {line.strip() for line in self.stdout_text(name_record).splitlines() if line.strip()}
        names_ok = expected.issubset(names)
        self.add_check(
            "crab-diff-name-only-delta",
            names_ok,
            {
                "expected_paths": sorted(expected),
                "names": sorted(names),
                "stdout_log": name_record.stdout_log,
            },
        )
        if not names_ok:
            raise WorkflowError("crab diff --name-only missed delta paths")

        path_filter = next((path for path in changed_paths if path.endswith("model-000.bin")), changed_paths[0])
        range_record = self.run_crab(
            repo,
            ["diff", "--byte-ranges", "--no-color", "HEAD~1", "HEAD", "--", path_filter],
            name="crab diff byte-ranges path filter",
        )
        range_text = self.stdout_text(range_record)
        range_lower = range_text.lower()
        range_ok = path_filter in range_text and "bytes" in range_lower
        range_ok = range_ok and "changed" in range_lower
        self.add_check(
            "crab-diff-byte-ranges-path-filter",
            range_ok,
            {
                "path": path_filter,
                "stdout": range_text[:2000],
                "stdout_log": range_record.stdout_log,
            },
        )
        if not range_ok:
            raise WorkflowError("crab diff --byte-ranges path filter output was not recognized")

        self.add_scenario(
            "crab-diff-operations",
            "ok",
            {"paths": sorted(expected), "summary": summary},
        )

    def run_delta_pull(self, source: Path, lazy: Path, manifest: dict[str, Any]) -> dict[str, Any]:
        updated = self.mutation(source, manifest, "delta", "delta")
        self.crab_add_paths(source, paths=updated["changed_paths"])
        self.commit(source, "e2e delta version")
        self.push(source)
        self.run_diff_checks(source, updated["changed_paths"])

        self.pull(lazy)
        self.hydrate_all(lazy)
        self.verify(lazy, updated, "delta-pull-hydrate-hashes")
        self.add_scenario(
            "delta-version-pull",
            "ok",
            {"files": len(updated["files"]), "bytes": updated["total_bytes"]},
        )
        return updated

    def run_team_non_overlap(self, manifest: dict[str, Any]) -> dict[str, Any]:
        alice = self.clone_repo("alice")
        bob = self.clone_repo("bob")
        self.hydrate_all(alice)
        self.hydrate_all(bob)

        alice_manifest = self.mutation(alice, manifest, "team_alice", "team-alice")
        self.crab_add_paths(alice, paths=alice_manifest["changed_paths"])
        self.commit(alice, "e2e alice non-overlap")
        self.push(alice)

        self.pull(bob)
        self.hydrate_all(bob)
        self.verify(bob, alice_manifest, "bob-after-alice-pull")

        bob_manifest = self.mutation(bob, alice_manifest, "team_bob", "team-bob")
        self.crab_add_paths(bob, paths=bob_manifest["changed_paths"])
        self.commit(bob, "e2e bob non-overlap")
        self.push(bob)

        self.pull(alice)
        self.hydrate_all(alice)
        self.verify(alice, bob_manifest, "alice-after-bob-pull")
        self.add_scenario("team-non-overlap", "ok", {})
        return bob_manifest

    def run_stale_push(self, manifest: dict[str, Any]) -> dict[str, Any]:
        stale = self.clone_repo("stale")
        alice = self.repos_dir / "alice"
        self.hydrate_all(stale)
        self.pull(alice)
        self.hydrate_all(alice)

        alice_manifest = self.mutation(alice, manifest, "stale_alice", "stale-alice")
        self.crab_add_paths(alice, paths=alice_manifest["changed_paths"])
        self.commit(alice, "e2e advance remote before stale push")
        self.push(alice)

        stale_manifest = self.mutation(stale, manifest, "stale_bob", "stale-bob")
        self.crab_add_paths(stale, paths=stale_manifest["changed_paths"])
        self.commit(stale, "e2e stale local edit")
        stale_failure = self.push(stale, expected_failure=True)
        self.classify_failure(
            stale_failure,
            "stale-push-classified",
            [
                "non-fast-forward",
                "not fast-forward",
                "fetch first",
                "rejected",
                "remote contains work",
            ],
        )

        expected_merged = self.merge_non_overlap_manifest(
            alice_manifest,
            stale_manifest,
            "stale-expected-merged",
        )
        self.pull(stale)
        self.hydrate_all(stale)
        self.verify(stale, expected_merged, "stale-merged-hashes")
        self.push(stale)
        self.add_scenario(
            "stale-push-retry",
            "ok",
            {
                "remote_advanced_manifest_bytes": alice_manifest["total_bytes"],
                "stale_manifest_bytes": stale_manifest["total_bytes"],
                "merged_manifest_bytes": expected_merged["total_bytes"],
            },
        )
        return expected_merged

    def run_conflict(self, manifest: dict[str, Any]) -> dict[str, Any]:
        conflict_a = self.clone_repo("conflict-a")
        conflict_b = self.clone_repo("conflict-b")
        self.hydrate_all(conflict_a)
        self.hydrate_all(conflict_b)

        a_manifest = self.mutation(conflict_a, manifest, "conflict_a", "conflict-a")
        self.crab_add_paths(conflict_a, paths=a_manifest["changed_paths"])
        self.commit(conflict_a, "e2e conflict side a")
        self.push(conflict_a)

        conflict_b_manifest = self.mutation(conflict_b, manifest, "conflict_b", "conflict-b")
        self.crab_add_paths(conflict_b, paths=conflict_b_manifest["changed_paths"])
        self.commit(conflict_b, "e2e conflict side b")
        self.pull(conflict_b, expected_failure=True)
        self.classify_unmerged(conflict_b, "same-file-conflict-classified")

        shutil.rmtree(conflict_b)
        recovered = self.clone_repo("conflict-b-recovered")
        self.hydrate_all(recovered)
        self.verify(recovered, a_manifest, "conflict-recovered-hashes")
        self.add_scenario("same-file-conflict-recovery", "ok", {})
        return a_manifest

    def run_ancillary_checks(self, repo: Path, stat_repo: Path) -> None:
        checks = [
            ("crab-fsck", ["crab", "fsck"], repo),
            ("crab-stat-json", ["crab", "stat", "--json"], stat_repo),
            ("crab-ls-files-json", ["crab", "ls-files", "--json"], repo),
            ("crab-du-local-json", ["crab", "du", "--json"], repo),
            ("crab-du-remote-json", ["crab", "du", "--remote", "--json"], repo),
            ("crab-status-json", ["crab", "status", "--json"], repo),
            ("crab-fetch-dry-run-json", ["crab", "fetch", "--dry-run", "--json"], repo),
            ("crab-fetch-all-dry-run-json", ["crab", "fetch", "--all", "--dry-run", "--json"], repo),
            ("crab-env-json", ["crab", "env", "--json"], repo),
            ("crab-version-json", ["crab", "version", "--json"], repo),
            ("crab-cache-stats", ["crab", "cache", "stats"], repo),
            ("crab-errors", ["crab", "errors"], repo),
            ("git-fsck", ["git", "fsck"], repo),
        ]
        failures = []
        for name, command, cwd in checks:
            record = self.run_cmd(name, command, cwd=cwd, check=False)
            ok = record.exit_code == 0
            if ok and name.endswith("-json"):
                try:
                    self.json_stdout(record)
                except (json.JSONDecodeError, WorkflowError) as exc:
                    ok = False
                    parse_error = str(exc)
                else:
                    parse_error = None
            else:
                parse_error = None
            self.add_check(
                name,
                ok,
                {
                    "cwd": str(cwd),
                    "stdout_log": record.stdout_log,
                    "stderr_log": record.stderr_log,
                    "exit_code": record.exit_code,
                    "parse_error": parse_error,
                },
            )
            if not ok:
                failures.append(name)

        if self.current_manifest:
            live_files = [
                entry["path"]
                for entry in self.current_manifest["files"]
                if not entry.get("deleted")
            ]
            if live_files:
                why_record = self.run_crab(
                    repo,
                    ["why", "--json", live_files[0]],
                    name="crab why json",
                    check=False,
                )
                ok = why_record.exit_code == 0
                parse_error = None
                if ok:
                    try:
                        self.json_stdout(why_record)
                    except (json.JSONDecodeError, WorkflowError) as exc:
                        ok = False
                        parse_error = str(exc)
                self.add_check(
                    "crab-why-json",
                    ok,
                    {
                        "path": live_files[0],
                        "repo": str(repo),
                        "stdout_log": why_record.stdout_log,
                        "stderr_log": why_record.stderr_log,
                        "exit_code": why_record.exit_code,
                        "parse_error": parse_error,
                    },
                )
                if not ok:
                    failures.append("crab-why-json")

        status = "ok" if not failures else "failed"
        self.add_scenario("ancillary-command-matrix", status, {"failures": failures})
        if failures:
            raise WorkflowError(f"ancillary checks failed: {', '.join(failures)}")

    def classify_failure(
        self,
        record: CommandRecord,
        check_name: str,
        patterns: list[str],
    ) -> None:
        text = ""
        for log in [record.stdout_log, record.stderr_log]:
            try:
                text += Path(log).read_text(errors="replace")
            except OSError:
                pass
        haystack = text.lower()
        matched = [pattern for pattern in patterns if pattern.lower() in haystack]
        self.add_check(
            check_name,
            bool(matched),
            {
                "command": record.name,
                "matched": matched,
                "patterns": patterns,
                "stdout_log": record.stdout_log,
                "stderr_log": record.stderr_log,
            },
        )
        if not matched:
            raise WorkflowError(
                f"{check_name} could not classify expected failure; "
                f"stderr={record.stderr_log}"
            )

    def classify_unmerged(self, repo: Path, check_name: str) -> None:
        record = self.run_git(repo, ["ls-files", "-u"], name="git ls-files unmerged")
        text = Path(record.stdout_log).read_text(errors="replace")
        paths = sorted(
            {
                line.split("\t", 1)[1]
                for line in text.splitlines()
                if "\t" in line
            }
        )
        self.add_check(
            check_name,
            bool(paths),
            {
                "repo": str(repo),
                "unmerged_paths": paths,
                "stdout_log": record.stdout_log,
            },
        )
        if not paths:
            raise WorkflowError(f"{check_name} found no unmerged paths")

    def run(self) -> int:
        only_modes = sum(
            (
                self.args.only_small_edit_push,
                self.args.only_multi_file_edit_push,
                self.args.only_dense_performance,
            )
        )
        if only_modes > 1:
            raise WorkflowError("choose only one --only-* benchmark mode")
        if self.args.only_dense_performance and self.args.profile != DENSE_PROFILE:
            raise WorkflowError(
                f"--only-dense-performance requires --profile {DENSE_PROFILE}"
            )
        self.setup_dirs()
        self.write_report()
        try:
            self.preflight()
            self.install_crab()

            source = self.repos_dir / "source"
            if self.args.only_small_edit_push:
                self.run_small_edit_push_benchmark()
                self.report.status = "ok"
                return 0
            if self.args.only_multi_file_edit_push:
                self.run_multi_file_edit_push_benchmark()
                self.report.status = "ok"
                return 0

            self.init_source_repo(source)
            manifest, initial_add, _initial_push = self.run_initial_push(source)
            if self.args.only_dense_performance:
                self.run_dense_performance_benchmark(source, manifest, initial_add)
                self.report.status = "ok"
                return 0
            self.run_cross_repo_dedup(source, manifest)
            lazy, eager = self.run_clone_checks(manifest)
            manifest = self.run_delta_pull(source, lazy, manifest)
            manifest = self.run_team_non_overlap(manifest)
            manifest = self.run_stale_push(manifest)
            manifest = self.run_conflict(manifest)
            self.current_manifest = manifest
            recovered = self.repos_dir / "conflict-b-recovered"
            self.run_ancillary_checks(recovered, source)
            self.scrub_macos_sidecars(self.run_root)
            sidecars = self.count_macos_sidecars()
            self.add_check(
                "macos-sidecars-scrubbed",
                sidecars == 0,
                {"count": sidecars},
            )
            if sidecars != 0:
                raise WorkflowError(f"found {sidecars} macOS sidecar file(s)")
            self.report.status = "ok"
            return 0
        except Exception as exc:
            self.report.status = "failed"
            self.report.scenarios.append(
                {
                    "name": "failure",
                    "status": "failed",
                    "detail": {"error": str(exc)},
                    "finished_at": utc_now(),
                }
            )
            print(f"error: {exc}", file=sys.stderr)
            return 1
        finally:
            self.report.finished_at = utc_now()
            self.write_report()
            print(f"Run ID: {self.run_id}")
            print(f"Remote: {self.remote_url}")
            print(f"Report: {self.artifacts_dir / 'report.json'}")


def clean(args: argparse.Namespace) -> int:
    run_id = safe_run_id(args.run_id)
    root = args.root.resolve()
    target = (root / run_id).resolve()
    if target.parent != root:
        raise WorkflowError(f"refusing to clean outside root: {target}")

    if target.exists():
        shutil.rmtree(target)
        print(f"removed local run directory: {target}")
    else:
        print(f"local run directory not found: {target}")

    aws = shutil.which("aws")
    if not aws:
        print("aws CLI not found; skipped remote prefix cleanup")
        return 0

    env = os.environ.copy()
    env.update(
        {
            "AWS_ACCESS_KEY_ID": "crab",
            "AWS_SECRET_ACCESS_KEY": "crab",
            "AWS_REGION": "us-east-1",
            "AWS_ENDPOINT_URL": args.endpoint_url,
            "AWS_ALLOW_HTTP": "true",
            "AWS_EC2_METADATA_DISABLED": "true",
            "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
        }
    )
    prefix = f"s3://{args.bucket}/{REMOTE_PREFIX}/{run_id}/"
    subprocess.run(
        [aws, "s3", "rm", prefix, "--recursive", "--endpoint-url", args.endpoint_url],
        env=env,
        check=False,
    )
    print(f"requested remote prefix cleanup: {prefix}")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    run = sub.add_parser("run", help="run the large-file workflow")
    run.add_argument("--profile", choices=sorted(fixture.PROFILES), default="smoke")
    run.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    run.add_argument("--bucket", default=DEFAULT_BUCKET)
    run.add_argument("--endpoint-url", default=DEFAULT_ENDPOINT)
    run.add_argument("--run-id")
    run.add_argument("--jobs", type=int, default=8)
    run.add_argument("--upload-concurrency", type=int, default=16)
    run.add_argument(
        "--only-small-edit-push",
        action="store_true",
        help="run only the one-large-file small-edit push benchmark",
    )
    run.add_argument(
        "--only-multi-file-edit-push",
        action="store_true",
        help="run only the multi-file second/third edit push benchmark",
    )
    run.add_argument(
        "--only-dense-performance",
        action="store_true",
        help=(
            "run only the 9.6 GiB add, cold/repeated global dedup, and "
            "cold/warm hydrate gates"
        ),
    )
    run.add_argument(
        "--small-edit-size-mib",
        type=positive_int,
        default=4608,
        help="large file size for --only-small-edit-push (default: 4.5 GiB)",
    )
    run.add_argument(
        "--small-edit-bytes",
        type=positive_int,
        default=100 * 1024,
        help="byte span overwritten before the second push",
    )
    run.add_argument(
        "--small-edit-upload-budget-mib",
        type=positive_int,
        default=64,
        help="maximum xorb bytes the second push may upload",
    )
    run.add_argument(
        "--small-edit-push-budget-secs",
        type=positive_int,
        default=60,
        help="maximum wall time for the second push",
    )
    run.add_argument(
        "--small-edit-max-rss-gib",
        type=positive_int,
        default=4,
        help="maximum RSS per measured add/push command",
    )
    run.add_argument(
        "--small-edit-min-plan-cover-ratio",
        type=ratio,
        default=0.90,
        help="minimum add-time plan coverage from existing/prepared chunks",
    )
    run.add_argument(
        "--multi-file-sizes",
        default="500m,1g,2g,5g,10g",
        help="comma-separated file sizes for --only-multi-file-edit-push",
    )
    run.add_argument(
        "--multi-seed-offset",
        type=int,
        default=0,
        help="offset added to deterministic multi-file fixture seeds",
    )
    run.add_argument(
        "--multi-edit-bytes",
        type=positive_int,
        default=100 * 1024,
        help="byte span overwritten in each file for the second and third edits",
    )
    run.add_argument(
        "--multi-edit-file-count",
        type=positive_int,
        help="number of multi-file inputs to edit in each edit round (default: all)",
    )
    run.add_argument(
        "--multi-add-all-candidates",
        action="store_true",
        help="after each multi-file edit, run crab add over every input path",
    )
    run.add_argument(
        "--multi-edit-upload-budget-mib",
        type=positive_int,
        default=1024,
        help="maximum xorb bytes each edit push may upload",
    )
    run.add_argument(
        "--multi-edit-push-budget-secs",
        type=positive_int,
        default=300,
        help="maximum wall time for each multi-file edit push",
    )
    run.add_argument(
        "--multi-edit-max-rss-gib",
        type=positive_int,
        default=8,
        help="maximum RSS per measured multi-file add/push command",
    )
    run.add_argument(
        "--multi-edit-min-plan-cover-ratio",
        type=ratio,
        default=0.90,
        help="minimum multi-file add-time plan coverage from existing/prepared chunks",
    )
    run.add_argument(
        "--measure-rss",
        action="store_true",
        help="wrap measured add/push/hydrate commands in /usr/bin/time -l",
    )
    chaos = run.add_mutually_exclusive_group()
    chaos.add_argument("--chaos", dest="chaos", action="store_true")
    chaos.add_argument("--no-chaos", dest="chaos", action="store_false")
    run.set_defaults(chaos=None)
    run.add_argument("--skip-install", action="store_true")
    run.add_argument(
        "--keep-workdirs",
        action="store_true",
        help="accepted for clarity; run directories are kept until clean is called",
    )
    run.add_argument("--chaos-kill-after", type=float, default=5.0)
    run.add_argument("--push-retries", type=int, default=8)

    clean_parser = sub.add_parser("clean", help="remove one run's local and remote state")
    clean_parser.add_argument("--run-id", required=True)
    clean_parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    clean_parser.add_argument("--bucket", default=DEFAULT_BUCKET)
    clean_parser.add_argument("--endpoint-url", default=DEFAULT_ENDPOINT)

    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.cmd == "run":
            return Runner(args).run()
        if args.cmd == "clean":
            return clean(args)
        raise WorkflowError(f"unhandled command {args.cmd}")
    except WorkflowError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
