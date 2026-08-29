#!/usr/bin/env python3
"""Measure independent Crab repository creation against a local S3 endpoint.

Each sample creates a fresh local Git repository, runs ``crab init`` against a
unique remote prefix, commits a small deterministic file set, and performs the
first native ``crab push``. Init always includes canonical layout and
generation-0 manifest publication. Cohorts vary file count, repository count,
and parallelism while sharing one isolated bucket so bucket cardinality is
visible in the report.

The benchmark intentionally does not delete its bucket or local run directory.
Use the reported AWS CLI commands to inspect or remove the isolated data after
reviewing the evidence.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DEFAULT_ENDPOINT = "http://127.0.0.1:9000"
DEFAULT_ROOT = Path("/Volumes/Workspace/CrabRepos")
DEFAULT_FILE_COUNTS = (1, 5, 10)
DEFAULT_REPO_COUNTS = (10, 50, 100)
DEFAULT_LATENCY_REPEATS = 3
DEFAULT_CONCURRENCY = 8
DEFAULT_FILE_BYTES = 4096
MAX_DIAGNOSTIC_BYTES = 4096


class BenchmarkError(RuntimeError):
    """Raised when the benchmark setup or verification is invalid."""


@dataclass
class RepoResult:
    cohort: str
    repo_index: int
    files_per_repo: int
    remote_url: str
    local_path: str
    status: str
    init_ms: float = 0.0
    file_write_ms: float = 0.0
    git_stage_ms: float = 0.0
    commit_ms: float = 0.0
    push_ms: float = 0.0
    total_ms: float = 0.0
    push_payload: dict[str, Any] | None = None
    error_stage: str | None = None
    error: str | None = None


@dataclass
class CohortResult:
    name: str
    kind: str
    files_per_repo: int
    requested_repos: int
    concurrency: int
    cumulative_repos_after: int
    elapsed_ms: float
    attempted: int
    successful: int
    failed: int
    attempted_repos_per_sec: float
    successful_repos_per_sec: float
    latency_ms: dict[str, dict[str, float]]
    object_inventory: dict[str, int]
    failures: list[dict[str, Any]] = field(default_factory=list)


@dataclass
class Report:
    schema: str
    version: str
    status: str
    run_id: str
    bucket: str
    remote_prefix: str
    endpoint_url: str
    root: str
    source_revision: str
    source_dirty: bool
    crab_binary: str
    crab_version: str
    rustfs_version: str
    host: dict[str, Any]
    matrix: dict[str, Any]
    cohorts: list[dict[str, Any]] = field(default_factory=list)
    artifacts: dict[str, str] = field(default_factory=dict)
    started_at: str = ""
    finished_at: str = ""
    error: str | None = None


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def make_run_id() -> str:
    return "repo-scale-" + datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def parse_positive_list(raw: str, flag: str) -> tuple[int, ...]:
    values: list[int] = []
    for item in raw.split(","):
        try:
            value = int(item.strip())
        except ValueError as exc:
            raise BenchmarkError(f"{flag} contains a non-integer: {item!r}") from exc
        if value <= 0:
            raise BenchmarkError(f"{flag} values must be positive: {value}")
        values.append(value)
    if not values:
        raise BenchmarkError(f"{flag} must contain at least one value")
    return tuple(dict.fromkeys(values))


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    weight = position - lower
    return ordered[lower] + (ordered[upper] - ordered[lower]) * weight


def summarize(values: list[float]) -> dict[str, float]:
    if not values:
        return {"min": 0.0, "avg": 0.0, "p50": 0.0, "p95": 0.0, "max": 0.0}
    return {
        "min": min(values),
        "avg": statistics.fmean(values),
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "max": max(values),
    }


def command_version(command: list[str]) -> str:
    result = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    return result.stdout.strip()[:MAX_DIAGNOSTIC_BYTES]


def resolve_executable(value: str, flag: str) -> str:
    path = Path(value)
    if path.is_file():
        return str(path.resolve())
    resolved = shutil.which(value)
    if resolved:
        return str(Path(resolved).resolve())
    raise BenchmarkError(f"{flag} executable not found: {value}")


def git_metadata(repo_root: Path) -> tuple[str, bool]:
    revision = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=repo_root,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    ).stdout.strip()
    dirty = bool(
        subprocess.run(
            ["git", "status", "--short"],
            cwd=repo_root,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        ).stdout.strip()
    )
    return revision or "unknown", dirty


def redact_error(stdout: str, stderr: str) -> str:
    text = "\n".join(part.strip() for part in (stderr, stdout) if part.strip())
    for secret in (
        os.environ.get("AWS_ACCESS_KEY_ID", ""),
        os.environ.get("AWS_SECRET_ACCESS_KEY", ""),
        os.environ.get("AWS_SESSION_TOKEN", ""),
    ):
        if secret:
            text = text.replace(secret, "<redacted>")
    return text[:MAX_DIAGNOSTIC_BYTES]


def parse_json_object(text: str) -> dict[str, Any] | None:
    for line in reversed(text.splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            return value
    return None


class RepoCreationBenchmark:
    def __init__(self, args: argparse.Namespace, repo_root: Path) -> None:
        self.args = args
        self.repo_root = repo_root
        self.run_id = args.run_id or make_run_id()
        self.run_root = args.root / self.run_id
        self.logs_root = self.run_root / "logs"
        self.cache_root = self.run_root / "cache"
        self.remote_prefix = self.run_id
        self.bucket = args.bucket or f"crab-perf-{self.run_id.removeprefix('repo-scale-')}"
        self.crab_bin = args.crab_bin
        self.env = self.build_env()
        revision, dirty = git_metadata(repo_root)
        self.report = Report(
            schema="crab.repo-creation-benchmark",
            version="1.0",
            status="running",
            run_id=self.run_id,
            bucket=self.bucket,
            remote_prefix=self.remote_prefix,
            endpoint_url=args.endpoint_url,
            root=str(self.run_root),
            source_revision=revision,
            source_dirty=dirty,
            crab_binary=self.crab_bin,
            crab_version=command_version([self.crab_bin, "--version"]),
            rustfs_version=command_version([args.rustfs_bin, "--version"]),
            host={
                "platform": platform.platform(),
                "machine": platform.machine(),
                "python": sys.version.split()[0],
                "cpu_count": os.cpu_count(),
            },
            matrix={
                "files_per_repo": list(args.file_counts),
                "throughput_repo_counts": list(args.repo_counts),
                "latency_repeats": args.latency_repeats,
                "throughput_concurrency": args.concurrency,
                "file_bytes": args.file_bytes,
                "remote_init": args.remote_init,
            },
            started_at=utc_now(),
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
                "CRAB_LOG": "error",
                "GIT_TERMINAL_PROMPT": "0",
                "NO_COLOR": "1",
            }
        )
        return env

    def write_report(self) -> None:
        artifacts = self.run_root / "artifacts"
        artifacts.mkdir(parents=True, exist_ok=True)
        path = artifacts / "report.json"
        temporary = path.with_suffix(".json.tmp")
        self.report.artifacts["report"] = str(path)
        temporary.write_text(json.dumps(asdict(self.report), indent=2, sort_keys=True) + "\n")
        temporary.replace(path)

    def run_command(
        self,
        args: list[str],
        cwd: Path,
        *,
        timeout: int,
        env: dict[str, str] | None = None,
    ) -> tuple[int, float, str, str]:
        started = time.perf_counter_ns()
        result = subprocess.run(
            args,
            cwd=cwd,
            env=env or self.env,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
        )
        elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
        return result.returncode, elapsed_ms, result.stdout, result.stderr

    def ensure_bucket(self) -> None:
        self.run_root.mkdir(parents=True, exist_ok=False)
        self.logs_root.mkdir()
        self.cache_root.mkdir()
        head = self.run_command(
            [
                self.args.aws_bin,
                "--endpoint-url",
                self.args.endpoint_url,
                "s3api",
                "head-bucket",
                "--bucket",
                self.bucket,
            ],
            self.run_root,
            timeout=self.args.timeout,
        )
        if head[0] == 0:
            if not self.args.allow_existing_bucket:
                raise BenchmarkError(
                    f"bucket already exists: {self.bucket}; use a unique --bucket or "
                    "--allow-existing-bucket"
                )
            return
        created = self.run_command(
            [
                self.args.aws_bin,
                "--endpoint-url",
                self.args.endpoint_url,
                "s3api",
                "create-bucket",
                "--bucket",
                self.bucket,
            ],
            self.run_root,
            timeout=self.args.timeout,
        )
        if created[0] != 0:
            raise BenchmarkError(
                f"failed to create bucket {self.bucket}: "
                f"{redact_error(created[2], created[3])}"
            )

    def inventory(self, prefix: str) -> dict[str, int]:
        result = self.run_command(
            [
                self.args.aws_bin,
                "--endpoint-url",
                self.args.endpoint_url,
                "s3api",
                "list-objects-v2",
                "--bucket",
                self.bucket,
                "--prefix",
                prefix,
                "--output",
                "json",
            ],
            self.run_root,
            timeout=self.args.timeout,
        )
        if result[0] != 0:
            raise BenchmarkError(
                f"inventory failed for prefix {prefix!r}: "
                f"{redact_error(result[2], result[3])}"
            )
        payload = json.loads(result[2] or "{}")
        contents = payload.get("Contents", [])
        return {
            "objects": len(contents),
            "bytes": sum(int(item.get("Size", 0)) for item in contents),
        }

    def make_repo_env(self, repo_index: int, cohort: str) -> dict[str, str]:
        env = self.env.copy()
        env["CRAB_CACHE_DIR"] = str(self.cache_root / cohort / f"repo-{repo_index:05d}")
        return env

    def write_files(self, repo: Path, cohort: str, repo_index: int, count: int) -> None:
        for file_index in range(count):
            seed = f"{self.run_id}:{cohort}:{repo_index}:{file_index}".encode()
            block = hashlib.sha256(seed).digest()
            content = (block * ((self.args.file_bytes + len(block) - 1) // len(block)))[
                : self.args.file_bytes
            ]
            (repo / f"file-{file_index:02d}.txt").write_bytes(content)

    def create_repo(self, cohort: str, repo_index: int, files_per_repo: int) -> RepoResult:
        remote_path = f"{self.remote_prefix}/{cohort}/repo-{repo_index:05d}"
        remote_url = f"crab://{self.bucket}/{remote_path}"
        repo = self.run_root / "repos" / cohort / f"repo-{repo_index:05d}"
        repo.mkdir(parents=True, exist_ok=False)
        env = self.make_repo_env(repo_index, cohort)
        result = RepoResult(
            cohort=cohort,
            repo_index=repo_index,
            files_per_repo=files_per_repo,
            remote_url=remote_url,
            local_path=str(repo),
            status="failed",
        )
        started = time.perf_counter_ns()

        try:
            init_args = [self.crab_bin, "init", "--json", remote_url]
            code, result.init_ms, stdout, stderr = self.run_command(
                init_args,
                repo,
                timeout=self.args.timeout,
                env=env,
            )
            if code != 0:
                raise BenchmarkError(f"init: {redact_error(stdout, stderr)}")

            write_started = time.perf_counter_ns()
            self.write_files(repo, cohort, repo_index, files_per_repo)
            result.file_write_ms = (time.perf_counter_ns() - write_started) / 1_000_000

            self.run_command(
                [self.args.git_bin, "config", "user.name", "Crab benchmark"],
                repo,
                timeout=self.args.timeout,
                env=env,
            )
            self.run_command(
                [self.args.git_bin, "config", "user.email", "benchmark@example.invalid"],
                repo,
                timeout=self.args.timeout,
                env=env,
            )

            code, result.git_stage_ms, stdout, stderr = self.run_command(
                [self.args.git_bin, "add", "-A"],
                repo,
                timeout=self.args.timeout,
                env=env,
            )
            if code != 0:
                raise BenchmarkError(f"git add: {redact_error(stdout, stderr)}")

            code, result.commit_ms, stdout, stderr = self.run_command(
                [
                    self.args.git_bin,
                    "commit",
                    "--quiet",
                    "-m",
                    f"create benchmark repo {repo_index}",
                ],
                repo,
                timeout=self.args.timeout,
                env=env,
            )
            if code != 0:
                raise BenchmarkError(f"git commit: {redact_error(stdout, stderr)}")

            code, result.push_ms, stdout, stderr = self.run_command(
                [
                    self.crab_bin,
                    "push",
                    "--json",
                    "--no-color",
                    "--lock-wait-secs",
                    "30",
                    "origin",
                    "HEAD:refs/heads/main",
                ],
                repo,
                timeout=self.args.push_timeout,
                env=env,
            )
            result.push_payload = parse_json_object(stdout)
            if code != 0:
                raise BenchmarkError(f"push: {redact_error(stdout, stderr)}")
            result.status = "ok"
        except (BenchmarkError, subprocess.TimeoutExpired) as exc:
            result.error_stage = result.error_stage or self.infer_stage(exc)
            result.error = str(exc)[:MAX_DIAGNOSTIC_BYTES]
        except OSError as exc:
            result.error_stage = "process"
            result.error = str(exc)[:MAX_DIAGNOSTIC_BYTES]
        finally:
            result.total_ms = (time.perf_counter_ns() - started) / 1_000_000
        return result

    @staticmethod
    def infer_stage(error: BaseException) -> str:
        text = str(error)
        for stage in ("init", "git add", "git commit", "push"):
            if text.startswith(stage):
                return stage.replace(" ", "_")
        if isinstance(error, subprocess.TimeoutExpired):
            return "timeout"
        return "unknown"

    def run_cohort(
        self,
        *,
        kind: str,
        files_per_repo: int,
        repo_count: int,
        concurrency: int,
    ) -> CohortResult:
        name = f"{kind}-f{files_per_repo:02d}-r{repo_count:04d}-c{concurrency:02d}"
        cohort_started = time.perf_counter_ns()
        worker = lambda index: self.create_repo(name, index, files_per_repo)
        results: list[RepoResult] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
            futures = [pool.submit(worker, index) for index in range(repo_count)]
            for future in concurrent.futures.as_completed(futures):
                results.append(future.result())
        results.sort(key=lambda item: item.repo_index)
        elapsed_ms = (time.perf_counter_ns() - cohort_started) / 1_000_000
        successful = [item for item in results if item.status == "ok"]
        elapsed_secs = max(elapsed_ms / 1000, 0.000001)
        inventory = self.inventory(f"{self.remote_prefix}/")
        latency_ms = {
            field_name: summarize([getattr(item, field_name) for item in successful])
            for field_name in ("init_ms", "file_write_ms", "git_stage_ms", "commit_ms", "push_ms", "total_ms")
        }
        failures = [
            {
                "repo_index": item.repo_index,
                "remote_url": item.remote_url,
                "error_stage": item.error_stage,
                "error": item.error,
            }
            for item in results
            if item.status != "ok"
        ]
        cohort = CohortResult(
            name=name,
            kind=kind,
            files_per_repo=files_per_repo,
            requested_repos=repo_count,
            concurrency=concurrency,
            cumulative_repos_after=sum(
                int(previous["successful"]) for previous in self.report.cohorts
            )
            + len(successful),
            elapsed_ms=elapsed_ms,
            attempted=len(results),
            successful=len(successful),
            failed=len(results) - len(successful),
            attempted_repos_per_sec=repo_count / elapsed_secs,
            successful_repos_per_sec=len(successful) / elapsed_secs,
            latency_ms=latency_ms,
            object_inventory=inventory,
            failures=failures,
        )
        self.report.cohorts.append(asdict(cohort))
        self.report.artifacts[f"{name}-results"] = self.write_cohort_results(name, results)
        self.write_report()
        if failures:
            raise BenchmarkError(f"cohort {name} had {len(failures)} failed repositories")
        return cohort

    def write_cohort_results(self, name: str, results: list[RepoResult]) -> str:
        path = self.run_root / "artifacts" / f"{name}-repos.json"
        path.write_text(json.dumps([asdict(item) for item in results], indent=2, sort_keys=True) + "\n")
        return str(path)

    def run(self) -> int:
        self.ensure_bucket()
        self.write_report()
        try:
            for files_per_repo in self.args.file_counts:
                if not self.args.skip_latency:
                    self.run_cohort(
                        kind="latency",
                        files_per_repo=files_per_repo,
                        repo_count=self.args.latency_repeats,
                        concurrency=1,
                    )
                if not self.args.skip_throughput:
                    for repo_count in self.args.repo_counts:
                        self.run_cohort(
                            kind="throughput",
                            files_per_repo=files_per_repo,
                            repo_count=repo_count,
                            concurrency=min(self.args.concurrency, repo_count),
                        )
        except (BenchmarkError, subprocess.TimeoutExpired, json.JSONDecodeError) as exc:
            self.report.status = "failed"
            self.report.error = str(exc)[:MAX_DIAGNOSTIC_BYTES]
            self.report.finished_at = utc_now()
            self.write_report()
            print(f"FAILED: {self.report.error}", file=sys.stderr)
            print(f"report: {self.report.artifacts.get('report', 'not-written')}", file=sys.stderr)
            return 1
        self.report.status = "ok"
        self.report.finished_at = utc_now()
        self.write_report()
        print(f"OK: {self.report.run_id}")
        print(f"bucket: {self.bucket}")
        print(f"remote prefix: {self.remote_prefix}/")
        print(f"run directory: {self.run_root}")
        print(f"report: {self.report.artifacts['report']}")
        print(f"cleanup: {self.cleanup_command()}")
        return 0

    def cleanup_command(self) -> str:
        return (
            f"AWS_ACCESS_KEY_ID={self.args.access_key} "
            f"AWS_SECRET_ACCESS_KEY=<secret> AWS_REGION={self.args.region} "
            f"aws --endpoint-url {self.args.endpoint_url} s3 rb s3://{self.bucket} --force"
        )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--bucket", help="isolated bucket name; defaults to a timestamped name")
    parser.add_argument("--run-id")
    parser.add_argument("--endpoint-url", default=DEFAULT_ENDPOINT)
    parser.add_argument("--access-key", default="crab")
    parser.add_argument("--secret-key", default="crab")
    parser.add_argument("--region", default="us-east-1")
    parser.add_argument("--crab-bin", default=shutil.which("crab") or "crab")
    parser.add_argument("--git-bin", default=shutil.which("git") or "git")
    parser.add_argument("--aws-bin", default=shutil.which("aws") or "aws")
    parser.add_argument("--rustfs-bin", default=shutil.which("rustfs") or "rustfs")
    parser.add_argument("--file-counts", default=','.join(map(str, DEFAULT_FILE_COUNTS)))
    parser.add_argument("--repo-counts", default=','.join(map(str, DEFAULT_REPO_COUNTS)))
    parser.add_argument("--latency-repeats", type=int, default=DEFAULT_LATENCY_REPEATS)
    parser.add_argument("--concurrency", type=int, default=DEFAULT_CONCURRENCY)
    parser.add_argument("--file-bytes", type=int, default=DEFAULT_FILE_BYTES)
    parser.add_argument("--timeout", type=int, default=120)
    parser.add_argument("--push-timeout", type=int, default=300)
    parser.add_argument("--allow-existing-bucket", action="store_true")
    parser.add_argument("--skip-latency", action="store_true")
    parser.add_argument("--skip-throughput", action="store_true")
    args = parser.parse_args()
    args.file_counts = parse_positive_list(args.file_counts, "--file-counts")
    args.repo_counts = parse_positive_list(args.repo_counts, "--repo-counts")
    if args.latency_repeats <= 0:
        parser.error("--latency-repeats must be positive")
    if args.concurrency <= 0:
        parser.error("--concurrency must be positive")
    if args.file_bytes <= 0:
        parser.error("--file-bytes must be positive")
    if args.skip_latency and args.skip_throughput:
        parser.error("at least one of latency or throughput must be enabled")
    for attribute, flag in (
        ("crab_bin", "--crab-bin"),
        ("git_bin", "--git-bin"),
        ("aws_bin", "--aws-bin"),
        ("rustfs_bin", "--rustfs-bin"),
    ):
        try:
            setattr(args, attribute, resolve_executable(getattr(args, attribute), flag))
        except BenchmarkError as exc:
            parser.error(str(exc))
    return args


def main() -> int:
    args = parse_args()
    repo_root = Path(__file__).resolve().parents[2]
    benchmark = RepoCreationBenchmark(args, repo_root)
    return benchmark.run()


if __name__ == "__main__":
    raise SystemExit(main())
