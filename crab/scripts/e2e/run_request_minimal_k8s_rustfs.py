#!/usr/bin/env python3
"""Qualify protocol v2 by replaying Kubernetes commits against RustFS."""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from run_concurrent_push_smoke import RequestCountingProxy


def now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def percentile(values: list[int], fraction: float) -> int:
    if not values:
        return 0
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


class Qualification:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.root = args.root.resolve() / args.run_id
        self.report_path = self.root / "artifacts" / "report.json"
        self.replay = self.root / "replay"
        self.incremental = self.root / "incremental-clone"
        self.final_clone = self.root / "final-clone"
        self.bin_dir = self.root / "bin"
        self.crab = self.bin_dir / "crab"
        self.remote_prefix = f"e2e-request-minimal/{args.run_id}"
        self.remote_url = f"crab://{args.bucket}/{self.remote_prefix}"
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
    ) -> tuple[int, dict[str, Any]]:
        before = self.proxy.snapshot()
        started = time.monotonic()
        result = subprocess.run(
            command,
            cwd=cwd,
            env=self.env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
        )
        elapsed_ms = round((time.monotonic() - started) * 1000)
        requests = RequestCountingProxy.delta(before, self.proxy.snapshot())
        if result.returncode:
            raise RuntimeError(
                f"command failed ({result.returncode}): {' '.join(command)}\n"
                f"stdout: {result.stdout[-2000:]}\nstderr: {result.stderr[-4000:]}"
            )
        return elapsed_ms, requests if meter else {}

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

        self.report = {
            "schema": "crab.request-minimal-k8s-rustfs",
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
            },
            "provenance": {
                "crab_binary": str(self.crab),
                "crab_version": version,
                "git_version": self.git(["--version"], self.replay),
            },
            "commit_oids": commits,
            "pushes": [],
            "maintenance": [],
            "correctness": {},
            "metrics": {},
        }
        self.save()

    def push(self, ordinal: int, oid: str) -> None:
        self.git(["update-ref", "refs/heads/main", oid], self.replay)
        elapsed, requests = self.run(
            [str(self.crab), "push", "--json", "origin", "main:refs/heads/main"],
            self.replay,
            meter=True,
        )
        self.report["pushes"].append(
            {
                "ordinal": ordinal,
                "oid": oid,
                "elapsed_ms": elapsed,
                "object_store": requests,
            }
        )
        self.save()

    def repack(self, ordinal: int, phase: str) -> None:
        elapsed, requests = self.run(
            [str(self.crab), "repack", "--json"], self.replay, meter=True, timeout=7200
        )
        self.report["maintenance"].append(
            {
                "ordinal": ordinal,
                "operation": f"repack-{phase}",
                "elapsed_ms": elapsed,
                "object_store": requests,
            }
        )
        self.save()

    def clone(self, target: Path, name: str, ordinal: int) -> None:
        elapsed, requests = self.run(
            [str(self.crab), "clone", "--lazy", self.remote_url, str(target)],
            self.root,
            meter=True,
            timeout=7200,
        )
        self.report["maintenance"].append(
            {
                "ordinal": ordinal,
                "operation": name,
                "elapsed_ms": elapsed,
                "object_store": requests,
            }
        )
        self.save()

    def fetch(self, ordinal: int, expected: str) -> None:
        elapsed, requests = self.run(
            [self.args.git_bin, "fetch", "origin"],
            self.incremental,
            meter=True,
            timeout=7200,
        )
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
            }
        )
        self.save()

    def summarize(self) -> None:
        pushes = self.report["pushes"]
        latencies = [item["elapsed_ms"] for item in pushes]
        requests = [item["object_store"]["requests"] for item in pushes]
        self.report["metrics"] = {
            "push_count": len(pushes),
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
                if ordinal % self.args.interval == 0:
                    self.fetch(ordinal, oid)
                    self.repack(ordinal, "interval")

            self.clone(self.final_clone, "final-clone", len(commits))
            expected = self.report["source"]["head"]
            actual = self.git(["rev-parse", "refs/remotes/origin/main"], self.final_clone)
            if actual != expected:
                raise RuntimeError(f"final clone tip {actual} does not match {expected}")
            self.git(["fsck", "--full"], self.final_clone, timeout=7200)
            self.report["correctness"] = {
                "final_tip": actual,
                "expected_tip": expected,
                "full_fsck": "passed",
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
