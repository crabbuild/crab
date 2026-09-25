#!/usr/bin/env python3
"""Qualify protocol v2 by replaying Kubernetes commits against RustFS."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import signal
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from run_concurrent_push_smoke import RequestCountingProxy


def now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def request_log_entry(run_operation: str, request: dict[str, Any]) -> dict[str, Any]:
    return {**request, "run_operation": run_operation}


def percentile(values: list[int], fraction: float) -> int:
    if not values:
        return 0
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def push_window_summaries(pushes: list[dict[str, Any]], window_size: int) -> list[dict[str, Any]]:
    windows: dict[int, list[dict[str, Any]]] = {}
    for push in pushes:
        ordinal = int(push["ordinal"])
        if ordinal == 0:
            continue
        start = ((ordinal - 1) // window_size) * window_size + 1
        windows.setdefault(start, []).append(push)
    summaries = []
    for start, entries in sorted(windows.items()):
        latencies = [int(item["elapsed_ms"]) for item in entries]
        requests = [int(item["object_store"]["requests"]) for item in entries]
        resources = [item["resources"] for item in entries if item.get("resources")]
        summaries.append(
            {
                "start_ordinal": start,
                "end_ordinal": start + len(entries) - 1,
                "push_count": len(entries),
                "latency_ms": {
                    "mean": round(sum(latencies) / len(latencies), 2),
                    "p50": percentile(latencies, 0.50),
                    "p95": percentile(latencies, 0.95),
                    "p99": percentile(latencies, 0.99),
                    "max": max(latencies),
                },
                "object_store_requests": {
                    "total": sum(requests),
                    "mean": round(sum(requests) / len(requests), 4),
                    "p50": percentile(requests, 0.50),
                    "p95": percentile(requests, 0.95),
                    "p99": percentile(requests, 0.99),
                    "max": max(requests),
                },
                "request_body_bytes": sum(
                    int(item["object_store"].get("request_body_bytes", 0)) for item in entries
                ),
                "response_body_bytes": sum(
                    int(item["object_store"].get("response_body_bytes", 0)) for item in entries
                ),
                "user_cpu_ms": sum(int(item.get("user_cpu_ms", 0)) for item in resources),
                "system_cpu_ms": sum(int(item.get("system_cpu_ms", 0)) for item in resources),
                "children_max_rss": max(
                    (int(item.get("children_max_rss", 0)) for item in resources), default=0
                ),
                "children_max_rss_unit": "bytes",
                "resource_sample_count": len(resources),
            }
        )
    return summaries


def git_auto_maintenance_events(trace_path: Path) -> list[list[str]]:
    if not trace_path.exists():
        return []
    events = []
    for line in trace_path.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        argv = event.get("argv")
        if event.get("event") != "child_start" or not isinstance(argv, list):
            continue
        if "--auto" in argv and any(arg in {"maintenance", "gc"} for arg in argv):
            events.append([str(arg) for arg in argv])
    return events


def git_pack_inventory(repository: Path) -> set[str]:
    pack_directory = repository / ".git" / "objects" / "pack"
    return {
        path.stem.removeprefix("pack-")
        for path in pack_directory.glob("pack-*.pack")
    }


def new_pack_ids(before: set[str], after: set[str]) -> list[str]:
    return sorted(after - before)


def require_at_most_one_new_pack(
    ordinal: int, before: set[str], after: set[str]
) -> list[str]:
    installed = new_pack_ids(before, after)
    if len(installed) > 1:
        raise RuntimeError(
            f"incremental fetch at {ordinal} installed {len(installed)} local packs"
        )
    return installed


def parse_repack_summary(stdout: str) -> dict[str, int]:
    envelope = json.loads(stdout)
    data = envelope.get("data")
    fields = (
        "packs_before",
        "packs_after",
        "bytes_before",
        "bytes_after",
        "bytes_read",
        "bytes_written",
        "elapsed_ms",
    )
    if not isinstance(data, dict) or any(field not in data for field in fields):
        raise RuntimeError("repack output is missing its structured summary")
    return {field: int(data[field]) for field in fields}


def sampled_blob_digests(
    git_bin: str, repository: Path, tip: str, *, limit: int = 32
) -> dict[str, dict[str, Any]]:
    env = os.environ.copy()
    env.update(
        {
            "GIT_NO_LAZY_FETCH": "1",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    listing = subprocess.run(
        [git_bin, "-C", str(repository), "ls-tree", "-r", "-l", "-z", tip],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    ).stdout
    candidates: list[tuple[bytes, str, int]] = []
    for row in listing.split(b"\0"):
        if not row:
            continue
        metadata, path = row.split(b"\t", 1)
        fields = metadata.split()
        if len(fields) != 4 or fields[1] != b"blob" or fields[3] == b"-":
            continue
        size = int(fields[3])
        if size > 4 * 1024 * 1024:
            continue
        candidates.append((path, fields[2].decode("ascii"), size))
    selected = sorted(candidates, key=lambda item: hashlib.sha256(item[0]).digest())[:limit]
    samples = {}
    for path, oid, size in selected:
        body = subprocess.run(
            [git_bin, "-C", str(repository), "cat-file", "blob", oid],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=True,
        ).stdout
        if len(body) != size:
            raise RuntimeError(f"sample blob {oid} has {len(body)} bytes, expected {size}")
        samples[path.decode("utf-8", errors="replace")] = {
            "oid": oid,
            "size": size,
            "sha256": hashlib.sha256(body).hexdigest(),
        }
    if not samples:
        raise RuntimeError(f"no small Git blobs found to sample at {tip}")
    return samples


class Qualification:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.root = args.root.resolve() / args.run_id
        self.report_path = self.root / "artifacts" / "report.json"
        self.raw_request_log = self.root / "artifacts" / "requests.jsonl"
        self.trace2_root = self.root / "artifacts" / "trace2"
        self.replay = self.root / "replay"
        self.incremental = self.root / "incremental-clone"
        self.final_clone = self.root / "final-clone"
        self.warm_clone = self.root / "warm-clone"
        self.bin_dir = self.root / "bin"
        self.crab = self.bin_dir / "crab"
        self.remote_prefix = f"e2e-capsule-protocol/{args.run_id}"
        self.remote_url = f"crab://{args.bucket}/{self.remote_prefix}"
        os.environ["CRAB_E2E_TRACE_REQUEST_PATHS"] = "1"
        self.proxy = RequestCountingProxy(
            args.endpoint_url,
            f"{args.bucket}/{self.remote_prefix}",
        )
        self.report: dict[str, Any] = {}

    def save(self) -> None:
        self.report_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.report_path.with_suffix(".tmp")
        temporary.write_text(json.dumps(self.report, indent=2, sort_keys=True) + "\n")
        temporary.replace(self.report_path)

    def env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "AWS_ACCESS_KEY_ID": self.args.access_key,
                "AWS_SECRET_ACCESS_KEY": self.args.secret_key,
                "AWS_REGION": self.args.region,
                "AWS_DEFAULT_REGION": self.args.region,
                "AWS_ENDPOINT_URL": self.proxy.url,
                "AWS_ENDPOINT_URL_S3": self.proxy.url,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "GIT_TERMINAL_PROMPT": "0",
                "CRAB_LOG": "error",
                "CRAB_CACHE_DIR": str(self.root / "cache"),
                "TMPDIR": str(self.root / "tmp"),
                "PATH": str(self.bin_dir) + os.pathsep + env.get("PATH", ""),
            }
        )
        return env

    def run(
        self,
        command: list[str],
        cwd: Path,
        *,
        timeout: int = 1800,
        meter: bool = False,
        sample_resources: bool = False,
        operation: str | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> tuple[int, dict[str, Any], dict[str, int] | None, str]:
        before = self.proxy.snapshot(include_paths=False)
        started = time.monotonic()
        env = self.env()
        if extra_env:
            env.update(extra_env)
        rss_peak = 0
        user_cpu_ms = 0
        system_cpu_ms = 0
        timed_out = False
        process: subprocess.Popen[bytes] | None = None
        with (
            tempfile.TemporaryFile(dir=self.root / "tmp") as stdout,
            tempfile.TemporaryFile(dir=self.root / "tmp") as stderr,
        ):
            try:
                process = subprocess.Popen(
                    command,
                    cwd=cwd,
                    env=env,
                    stdout=stdout,
                    stderr=stderr,
                    start_new_session=os.name != "nt",
                    creationflags=(
                        subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
                    ),
                )
                while process.poll() is None:
                    if sample_resources:
                        rss, user, system = self.process_tree_resources(process.pid)
                        rss_peak = max(rss_peak, rss)
                        user_cpu_ms = max(user_cpu_ms, user)
                        system_cpu_ms = max(system_cpu_ms, system)
                    remaining = timeout - (time.monotonic() - started)
                    if remaining <= 0:
                        self.terminate_process(process)
                        timed_out = True
                        break
                    try:
                        process.wait(timeout=min(0.05, remaining))
                    except subprocess.TimeoutExpired:
                        pass
                exit_code = process.wait()
            except BaseException:
                if process is not None and process.poll() is None:
                    self.terminate_process(process)
                    process.wait()
                raise
            stdout.seek(0)
            stdout_text = stdout.read().decode("utf-8", errors="replace")
            stderr.seek(0)
            stderr_text = stderr.read().decode("utf-8", errors="replace")
        elapsed_ms = round((time.monotonic() - started) * 1000)
        requests = RequestCountingProxy.delta(
            before, self.proxy.snapshot(include_paths=False)
        )
        for request in self.proxy.paths_since(before.get("path_count", 0)):
            self.raw_request_log.parent.mkdir(parents=True, exist_ok=True)
            with self.raw_request_log.open("a", encoding="utf-8") as raw_log:
                raw_log.write(
                    json.dumps(
                        request_log_entry(operation or Path(command[0]).name, request),
                        sort_keys=True,
                    )
                    + "\n"
                )
        if timed_out:
            raise RuntimeError(
                f"command timed out after {timeout}s: {' '.join(command)}\n"
                f"stdout: {stdout_text[-2000:]}\nstderr: {stderr_text[-4000:]}"
            )
        if exit_code:
            raise RuntimeError(
                f"command failed ({exit_code}): {' '.join(command)}\n"
                f"stdout: {stdout_text[-2000:]}\nstderr: {stderr_text[-4000:]}"
            )
        resources = (
            {
                "user_cpu_ms": user_cpu_ms,
                "system_cpu_ms": system_cpu_ms,
                "children_max_rss": rss_peak,
                "children_max_rss_unit": "bytes",
            }
            if sample_resources
            else None
        )
        return elapsed_ms, requests if meter else {}, resources, stdout_text

    def trace_path(self, operation: str) -> Path:
        self.trace2_root.mkdir(parents=True, exist_ok=True)
        return self.trace2_root / f"{operation}.jsonl"

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

    @staticmethod
    def terminate_process(process: subprocess.Popen[bytes]) -> None:
        if os.name == "nt":
            process.kill()
        else:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def git(self, args: list[str], cwd: Path, *, timeout: int = 1800) -> str:
        result = subprocess.run(
            [self.args.git_bin, *args],
            cwd=cwd,
            env=self.env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
        if result.returncode:
            raise RuntimeError(
                f"git failed ({result.returncode}): {' '.join(args)}\n"
                f"stdout: {result.stdout[-2000:]}\nstderr: {result.stderr[-4000:]}"
            )
        return result.stdout.strip()

    def initialize(self) -> None:
        if self.root.exists():
            raise RuntimeError(f"run root already exists: {self.root}")
        (self.root / "tmp").mkdir(parents=True)
        self.bin_dir.mkdir()
        shutil.copy2(Path(self.args.crab_bin).resolve(), self.crab)
        self.crab.chmod(0o755)
        (self.bin_dir / "git-remote-crab").symlink_to(self.crab)

        source = Path(self.args.source).resolve()
        head = self.git(["rev-parse", "HEAD"], source)
        base = self.git(["rev-parse", f"HEAD~{self.args.commits}"], source)
        commits = self.git(
            ["rev-list", "--first-parent", "--reverse", f"{base}..{head}"], source
        ).splitlines()
        if len(commits) != self.args.commits:
            raise RuntimeError(f"expected {self.args.commits} commits, got {len(commits)}")

        self.git(["clone", "--shared", "--no-checkout", str(source), str(self.replay)], self.root)
        self.git(["remote", "remove", "origin"], self.replay)
        self.git(["symbolic-ref", "HEAD", "refs/heads/main"], self.replay)
        self.git(["update-ref", "refs/heads/main", base], self.replay)
        self.run([str(self.crab), "init", self.remote_url], self.replay)
        staging_source = None
        if self.args.staging_source is not None:
            self.copy_staging_source(self.args.staging_source)
            staging_source = str(self.args.staging_source.resolve())
        self.git(["remote", "set-url", "origin", self.remote_url], self.replay)

        version = subprocess.run(
            [str(self.crab), "--version"],
            cwd=self.replay,
            env=self.env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=True,
        ).stdout.strip()
        binary_sha256 = hashlib.sha256(self.crab.read_bytes()).hexdigest()

        self.report = {
            "schema": "crab.capsule-protocol-k8s-rustfs",
            "version": "1.0",
            "status": "running",
            "started_at": now(),
            "source": {"path": str(source), "head": head, "base": base},
            "remote": {
                "url": self.remote_url,
                "endpoint": self.args.endpoint_url,
                "bucket": self.args.bucket,
                "prefix": self.remote_prefix,
            },
            "workload": {
                "commits": self.args.commits,
                "fetch_interval": self.args.interval,
                "repack_interval": self.args.interval,
                **({"staging_source": staging_source} if staging_source else {}),
            },
            "provenance": {
                "crab_binary": str(self.crab),
                "crab_version": version,
                "crab_sha256": binary_sha256,
                "git_version": self.git(["--version"], self.replay),
            },
            "artifacts": {
                "report": str(self.report_path),
                "object_store_requests": str(self.raw_request_log),
                "git_trace2_directory": str(self.trace2_root),
            },
            "commit_oids": commits,
            "pushes": [],
            "maintenance": [],
            "correctness": {},
            "metrics": {},
        }
        self.save()

    def copy_staging_source(self, source: Path) -> None:
        source = source.resolve()
        destination = (self.replay / ".crab" / "staging").resolve()
        if not source.is_dir():
            raise RuntimeError(f"staging source is not a directory: {source}")
        if source == destination or source in destination.parents:
            raise RuntimeError("staging source must be outside the replay checkout")

        def ignore_ephemeral(_directory: str, names: list[str]) -> set[str]:
            return {name for name in names if name in {"lockfile", "index.db-shm", "index.db-wal"}}

        shutil.copytree(
            source,
            destination,
            ignore=ignore_ephemeral,
        )

    def push(self, ordinal: int, oid: str) -> None:
        self.git(["update-ref", "refs/heads/main", oid], self.replay)
        self.trace2_root.mkdir(parents=True, exist_ok=True)
        trace_path = self.trace_path(f"push-{ordinal:05}")
        elapsed, requests, resources, _ = self.run(
            [str(self.crab), "push", "--json", "origin", "main:refs/heads/main"],
            self.replay,
            meter=True,
            sample_resources=ordinal > 0 and ordinal % self.args.interval == 1,
            operation=f"push-{ordinal:05}",
            extra_env={"GIT_TRACE2_EVENT": str(trace_path)},
        )
        push = {
            "ordinal": ordinal,
            "oid": oid,
            "elapsed_ms": elapsed,
            "object_store": requests,
        }
        if resources is not None:
            push["resources"] = resources
        self.report["pushes"].append(push)
        if ordinal == 0:
            self.save()

    def repack(self, ordinal: int, phase: str) -> None:
        name = f"repack-{phase}-{ordinal:05}"
        elapsed, requests, resources, stdout = self.run(
            [str(self.crab), "repack", "--json"],
            self.replay,
            meter=True,
            sample_resources=True,
            timeout=7200,
            operation=name,
            extra_env={"GIT_TRACE2_EVENT": str(self.trace_path(name))},
        )
        summary = parse_repack_summary(stdout)
        self.report["maintenance"].append(
            {
                "ordinal": ordinal,
                "operation": f"repack-{phase}",
                "elapsed_ms": elapsed,
                "object_store": requests,
                "repack": summary,
                "resources": resources,
            }
        )
        self.save()

    def clone(
        self,
        target: Path,
        name: str,
        ordinal: int,
        *,
        cache_name: str | None = None,
    ) -> None:
        cache = self.root / "cache" / (cache_name or name)
        cache.mkdir(parents=True, exist_ok=True)
        elapsed, requests, resources, _ = self.run(
            [str(self.crab), "clone", "--lazy", self.remote_url, str(target)],
            self.root,
            meter=True,
            sample_resources=True,
            timeout=7200,
            operation=name,
            extra_env={
                "CRAB_CACHE_DIR": str(cache),
                "GIT_TRACE2_EVENT": str(self.trace_path(name)),
            },
        )
        self.report["maintenance"].append(
            {
                "ordinal": ordinal,
                "operation": name,
                "elapsed_ms": elapsed,
                "object_store": requests,
                "resources": resources,
            }
        )
        self.save()

    def fetch(self, ordinal: int, expected: str) -> None:
        packs_before = git_pack_inventory(self.incremental)
        name = f"incremental-fetch-{ordinal:05}"
        elapsed, requests, resources, _ = self.run(
            [self.args.git_bin, "fetch", "origin"],
            self.incremental,
            meter=True,
            sample_resources=True,
            timeout=7200,
            operation=name,
            extra_env={"GIT_TRACE2_EVENT": str(self.trace_path(name))},
        )
        packs_after = git_pack_inventory(self.incremental)
        installed_packs = require_at_most_one_new_pack(ordinal, packs_before, packs_after)
        actual = self.git(["rev-parse", "refs/remotes/origin/main"], self.incremental)
        if actual != expected:
            raise RuntimeError(f"fetch at {ordinal} returned {actual}, expected {expected}")
        self.git(["fsck", "--connectivity-only"], self.incremental, timeout=7200)
        self.report["maintenance"].append(
            {
                "ordinal": ordinal,
                "operation": "incremental-fetch",
                "elapsed_ms": elapsed,
                "object_store": requests,
                "tip": actual,
                "new_local_packs": installed_packs,
                "local_pack_count": len(packs_after),
                "resources": resources,
            }
        )
        self.save()

    def summarize(self) -> None:
        seed = self.report["pushes"][0]
        pushes = self.report["pushes"][1:]
        if len(pushes) != self.args.commits:
            raise RuntimeError(f"expected {self.args.commits} incremental pushes, got {len(pushes)}")
        fetches = [
            item for item in self.report["maintenance"] if item["operation"] == "incremental-fetch"
        ]
        expected_fetches = self.args.commits // self.args.interval
        if len(fetches) != expected_fetches:
            raise RuntimeError(f"expected {expected_fetches} incremental fetches, got {len(fetches)}")
        latencies = [item["elapsed_ms"] for item in pushes]
        requests = [item["object_store"]["requests"] for item in pushes]
        auto_events = [
            event
            for path in sorted(self.trace2_root.glob("*.jsonl"))
            for event in git_auto_maintenance_events(path)
        ]
        if auto_events:
            raise RuntimeError(f"Git ran automatic maintenance during qualification: {auto_events}")
        self.report["metrics"] = {
            "seed": {
                "elapsed_ms": seed["elapsed_ms"],
                "object_store_requests": seed["object_store"]["requests"],
            },
            "incremental_push_count": len(pushes),
            "push_latency_ms": {
                "mean": round(sum(latencies) / len(latencies), 2),
                "p50": percentile(latencies, 0.50),
                "p95": percentile(latencies, 0.95),
                "p99": percentile(latencies, 0.99),
                "max": max(latencies),
            },
            "push_object_store_requests": {
                "total": sum(requests),
                "mean": round(sum(requests) / len(requests), 4),
                "p50": percentile(requests, 0.50),
                "p95": percentile(requests, 0.95),
                "p99": percentile(requests, 0.99),
                "max": max(requests),
                "under_10_average": sum(requests) / len(requests) < 10,
            },
            "push_windows": push_window_summaries(pushes, self.args.interval),
            "git_auto_maintenance_events": auto_events,
        }
        self.report["status"] = "passed"
        self.report["finished_at"] = now()
        self.save()

    def execute(self) -> None:
        self.proxy.start()
        try:
            self.initialize()
            base = self.report["source"]["base"]
            self.push(0, base)
            self.repack(0, "seed")
            self.clone(self.incremental, "incremental-clone", 0)

            commits: list[str] = self.report["commit_oids"]
            for ordinal, oid in enumerate(commits, 1):
                self.push(ordinal, oid)
                if ordinal % 100 == 0:
                    print(
                        f"[{now()}] replayed {ordinal}/{len(commits)} commits",
                        flush=True,
                    )
                    self.save()
                if ordinal % self.args.interval == 0:
                    self.fetch(ordinal, oid)
                    self.repack(ordinal, "interval")

            self.clone(
                self.final_clone,
                "cold-final-clone",
                len(commits),
                cache_name="final-clones",
            )
            expected = self.report["source"]["head"]
            actual = self.git(["rev-parse", "refs/remotes/origin/main"], self.final_clone)
            if actual != expected:
                raise RuntimeError(f"cold clone tip {actual} does not match {expected}")
            self.git(["fsck", "--strict", "--full"], self.final_clone, timeout=7200)
            source_samples = sampled_blob_digests(
                self.args.git_bin, Path(self.args.source).resolve(), expected
            )
            cold_samples = sampled_blob_digests(self.args.git_bin, self.final_clone, expected)
            if cold_samples != source_samples:
                raise RuntimeError("cold clone sampled Git blob bytes differ from source")
            self.clone(
                self.warm_clone,
                "warm-final-clone",
                len(commits),
                cache_name="final-clones",
            )
            warm_tip = self.git(
                ["rev-parse", "refs/remotes/origin/main"], self.warm_clone
            )
            if warm_tip != expected:
                raise RuntimeError(f"warm clone tip {warm_tip} does not match {expected}")
            self.git(["fsck", "--strict", "--full"], self.warm_clone, timeout=7200)
            warm_samples = sampled_blob_digests(self.args.git_bin, self.warm_clone, expected)
            if warm_samples != source_samples:
                raise RuntimeError("warm clone sampled Git blob bytes differ from source")
            fsck_ms, fsck_requests, _, _ = self.run(
                [str(self.crab), "fsck", "--jsonl"],
                self.replay,
                meter=True,
                timeout=7200,
                operation="remote-crab-fsck",
            )
            self.report["maintenance"].append(
                {
                    "ordinal": len(commits),
                    "operation": "remote-crab-fsck",
                    "elapsed_ms": fsck_ms,
                    "object_store": fsck_requests,
                }
            )
            self.save()
            self.report["correctness"] = {
                "final_tip": actual,
                "warm_clone_tip": warm_tip,
                "expected_tip": expected,
                "cold_clone_strict_full_git_fsck": "passed",
                "warm_clone_strict_full_git_fsck": "passed",
                "remote_crab_fsck": "passed",
                "sampled_blob_count": len(source_samples),
                "cold_clone_sampled_blob_bytes": "matched source",
                "warm_clone_sampled_blob_bytes": "matched source",
                "incremental_fetches": self.args.commits // self.args.interval,
            }
            self.summarize()
        except BaseException as error:
            if self.report:
                self.report["status"] = "failed"
                self.report["finished_at"] = now()
                self.report["error"] = str(error)
                self.save()
            raise
        finally:
            self.proxy.close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True)
    parser.add_argument(
        "--staging-source",
        type=Path,
        help="Optional existing .crab/staging directory for pointer-bearing replay commits",
    )
    parser.add_argument("--crab-bin", required=True)
    parser.add_argument("--git-bin", default="git")
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--endpoint-url", default="http://127.0.0.1:9000")
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--access-key", default="crab")
    parser.add_argument("--secret-key", default="crab")
    parser.add_argument("--region", default="us-east-1")
    parser.add_argument("--commits", type=int, default=5000)
    parser.add_argument("--interval", type=int, default=500)
    args = parser.parse_args()
    if args.commits <= 0 or args.interval <= 0 or args.commits % args.interval:
        parser.error("commits must be positive and divisible by interval")
    return args


if __name__ == "__main__":
    Qualification(parse_args()).execute()
