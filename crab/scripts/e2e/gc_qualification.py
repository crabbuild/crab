#!/usr/bin/env python3
"""Run the GC qualification gate and write redacted evidence.

This runner is intentionally fail-closed. A provider is ``unsupported`` unless
the operator supplies an isolated endpoint and an explicit end-to-end command
that creates the fixture, runs GC, fsck, and a fresh clone/readback. The local
RustFS/unit probe is supplementary evidence and never upgrades a production
provider claim.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


EVIDENCE_SCHEMA = 1
SECRET_KEYS = {
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "AZURE_CLIENT_SECRET",
    "AZURE_STORAGE_KEY",
}
RUN_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
REQUIRED_CHECKS = {
    "live_objects_preserved",
    "unreachable_objects_deleted",
    "fsck_after_gc",
    "fresh_clone_readback",
    "writer_race",
    "resume_after_delete_crash",
    "resume_after_journal_crash",
    "bounded_memory",
    "bounded_writer_pause",
}


@dataclass(frozen=True)
class FixtureObject:
    key: str
    size: int
    live: bool
    digest: str


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def safe_run_id(value: str) -> str:
    if not RUN_ID_RE.fullmatch(value):
        raise ValueError("run id may contain only letters, numbers, dot, underscore, or dash")
    return value


def fixture_objects(seed: int, cardinality: int) -> list[FixtureObject]:
    if cardinality <= 0:
        raise ValueError("cardinality must be positive")
    objects: list[FixtureObject] = []
    for index in range(cardinality):
        payload = f"gc-fixture:{seed}:{index}".encode()
        object_hash = hashlib.blake2b(payload, digest_size=32).hexdigest()
        objects.append(
            FixtureObject(
                key=f".crab/xorbs/{object_hash[:2]}/{object_hash}",
                size=1024 + (index % 4096),
                live=index % 5 != 0,
                digest=hashlib.sha256(payload).hexdigest(),
            )
        )
    return objects


def redacted_environment() -> dict[str, str]:
    result: dict[str, str] = {}
    for key in ("AWS_ENDPOINT_URL", "AWS_REGION", "GC_QUALIFICATION_COMMAND"):
        if key in os.environ:
            result[key] = "<redacted>" if key in SECRET_KEYS else os.environ[key]
    return result


def redact_text(value: str) -> str:
    for key in SECRET_KEYS:
        secret = os.environ.get(key)
        if secret:
            value = value.replace(secret, "<redacted>")
    return value


def command_record(
    command: list[str], cwd: Path, timeout: int, extra_env: dict[str, str]
) -> dict[str, Any]:
    started = time.monotonic()
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout,
            env={**os.environ, **extra_env},
        )
        status = completed.returncode == 0
        stdout_tail = redact_text(completed.stdout[-2000:])
        stderr_tail = redact_text(completed.stderr[-2000:])
        error = None if status else (completed.stderr or completed.stdout)[-2000:]
        return {
            "command": command,
            "exit_code": completed.returncode,
            "duration_ms": int((time.monotonic() - started) * 1000),
            "status": "passed" if status else "failed",
            "stdout_tail": stdout_tail,
            "stderr_tail": stderr_tail,
            "error": redact_text(error) if error else None,
        }
    except subprocess.TimeoutExpired:
        return {
            "command": command,
            "exit_code": None,
            "duration_ms": int((time.monotonic() - started) * 1000),
            "status": "failed",
            "stdout_tail": "",
            "stderr_tail": "",
            "error": "qualification command timed out",
        }


def make_report(
    run_id: str,
    provider: str,
    endpoint: str | None,
    fixture: list[FixtureObject],
    work_dir: Path,
) -> dict[str, Any]:
    return {
        "schema_version": EVIDENCE_SCHEMA,
        "run_id": run_id,
        "status": "blocked",
        "qualification_level": "supplementary",
        "provider": provider,
        "endpoint_class": "unknown" if endpoint is None else "configured",
        "scope": f"gc-qualification/{run_id}",
        "started_at": utc_now(),
        "finished_at": "",
        "work_dir": str(work_dir),
        "environment": redacted_environment(),
        "fixture": {
            "seed": None,
            "objects": len(fixture),
            "logical_bytes": sum(item.size for item in fixture),
            "live_objects": sum(item.live for item in fixture),
            "unreachable_objects": sum(not item.live for item in fixture),
        },
        "metrics": {
            "peak_rss_bytes": None,
            "temporary_bytes": None,
            "open_files_high_water": None,
            "list_requests": None,
            "head_requests": None,
            "get_requests": None,
            "delete_requests": None,
            "referenced_shard_body_gets": None,
        },
        "checks": [],
        "commands": [],
        "artifacts": {"fixture": str(work_dir / "fixture.json")},
        "unsupported_reason": None,
    }


def run_provider(args: argparse.Namespace, provider: str) -> Path:
    run_id = safe_run_id(args.run_id)
    provider_dir = args.work_dir / provider / run_id
    provider_dir.mkdir(parents=True, exist_ok=True)
    fixture = fixture_objects(args.seed, args.cardinality)
    fixture_path = provider_dir / "fixture.json"
    fixture_path.write_text(
        json.dumps(
            {
                "schema_version": EVIDENCE_SCHEMA,
                "seed": args.seed,
                "objects": [item.__dict__ for item in fixture],
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    endpoint = args.endpoint_url or os.environ.get("AWS_ENDPOINT_URL")
    report = make_report(run_id, provider, endpoint, fixture, provider_dir)
    report["fixture"]["seed"] = args.seed

    if not endpoint:
        report["unsupported_reason"] = (
            "no isolated provider endpoint supplied; destructive qualification was not run"
        )
    elif args.command:
        report["qualification_level"] = "end_to_end"
        report["status"] = "running"
        command = [part for part in args.command]
        result_path = provider_dir / "result.json"
        record = command_record(
            command,
            provider_dir,
            args.timeout,
            {
                "CRAB_GC_QUALIFICATION_FIXTURE": str(fixture_path),
                "CRAB_GC_QUALIFICATION_RESULT": str(result_path),
                "CRAB_GC_QUALIFICATION_SCOPE": report["scope"],
            },
        )
        report["commands"].append(record)
        if record["status"] != "passed":
            report["status"] = "failed"
            report["unsupported_reason"] = record["error"]
        else:
            try:
                result = json.loads(result_path.read_text(encoding="utf-8"))
                checks = result["checks"]
                check_names = {
                    check.get("name")
                    for check in checks
                    if isinstance(check, dict) and check.get("status") == "passed"
                }
                missing_checks = REQUIRED_CHECKS.difference(check_names)
                if missing_checks:
                    raise ValueError(
                        f"qualification result is missing checks: {sorted(missing_checks)}"
                    )
                report["checks"] = checks
                report["metrics"] = result["metrics"]
                report["artifacts"].update(result["artifacts"])
                report["status"] = "passed"
            except (OSError, KeyError, ValueError, json.JSONDecodeError) as error:
                report["status"] = "failed"
                report["unsupported_reason"] = str(error)
    else:
        report["unsupported_reason"] = (
            "endpoint is configured but no explicit fixture/GC/fsck/clone command was supplied"
        )

    report["finished_at"] = utc_now()
    report_path = provider_dir / "evidence.json"
    report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return report_path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", action="append", dest="providers")
    parser.add_argument("--providers", dest="provider_list", help="comma-separated provider names")
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--run-id", default=datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S"))
    parser.add_argument("--seed", type=int, default=20260822)
    parser.add_argument("--cardinality", type=int, default=10000)
    parser.add_argument("--endpoint-url")
    parser.add_argument("--command", nargs="+", help="explicit isolated end-to-end command")
    parser.add_argument("--timeout", type=int, default=1800)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    providers = list(args.providers or [])
    if args.provider_list:
        providers.extend(item.strip() for item in args.provider_list.split(",") if item.strip())
    if not providers:
        providers = ["rustfs"]
    try:
        paths = [run_provider(args, provider) for provider in sorted(set(providers))]
    except (OSError, ValueError) as error:
        print(f"gc qualification failed: {error}", file=sys.stderr)
        return 2
    for path in paths:
        print(path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
