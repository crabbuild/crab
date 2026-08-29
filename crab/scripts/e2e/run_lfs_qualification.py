#!/usr/bin/env python3
"""Qualify the Git LFS standalone-direct profile against a Crab remote.

The harness deliberately drives the user-visible Git/Git LFS workflow. It
generates content in bounded chunks, records redacted command/evidence data,
and refuses to reuse an existing repository or evidence directory. A caller
must provide a unique remote prefix and a capacity-qualified evidence root.

The default workload is a small local smoke. Larger workloads require
explicit flags, which prevents an accidental multi-gigabyte run in CI or a
developer checkout.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import resource
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Sequence
from urllib.parse import urlsplit, urlunsplit


SCHEMA = "crab.lfs-qualification"
VERSION = "1.0"
CHUNK_SIZE = 1024 * 1024
DEFAULT_MIN_SIZE = 1024 * 1024
DEFAULT_MAX_SIZE = 2 * 1024 * 1024
SECRET_ENV_KEYS = {"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"}


class QualificationError(RuntimeError):
    """Raised when the harness cannot produce trustworthy evidence."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def redact_url(value: str) -> str:
    """Remove URL credentials while preserving the endpoint identity."""
    try:
        parsed = urlsplit(value)
    except ValueError:
        return "<redacted-url>"
    if parsed.username is None and parsed.password is None:
        return value
    host = parsed.hostname or ""
    if parsed.port is not None:
        host = f"{host}:{parsed.port}"
    return urlunsplit((parsed.scheme, host, parsed.path, parsed.query, parsed.fragment))


def redact_text(value: str, secrets: Sequence[str]) -> str:
    result = value
    for secret in sorted({item for item in secrets if item}, key=len, reverse=True):
        result = result.replace(secret, "<redacted>")
    return redact_url(result)


def executable(value: str, label: str) -> str:
    resolved = shutil.which(value) if not Path(value).is_absolute() else value
    if resolved is None or not Path(resolved).is_file() or not os.access(resolved, os.X_OK):
        raise QualificationError(f"{label} is not executable: {value}")
    return str(Path(resolved).resolve())


def deterministic_block(seed: str, object_index: int, block_index: int) -> bytes:
    return hashlib.sha256(
        f"crab-lfs:{seed}:{object_index}:{block_index}".encode("utf-8")
    ).digest()


def write_deterministic(path: Path, size: int, seed: str, object_index: int) -> str:
    """Write and hash one payload without retaining the payload in memory."""
    path.parent.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256()
    remaining = size
    block_index = 0
    with path.open("wb") as handle:
        while remaining:
            block = deterministic_block(seed, object_index, block_index)
            chunk = (block * ((min(remaining, CHUNK_SIZE) + len(block) - 1) // len(block)))[:
                min(remaining, CHUNK_SIZE)
            ]
            handle.write(chunk)
            digest.update(chunk)
            remaining -= len(chunk)
            block_index += 1
    return digest.hexdigest()


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(CHUNK_SIZE), b""):
            digest.update(chunk)
    return digest.hexdigest()


class Qualification:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.crab = executable(args.crab_bin, "Crab binary")
        self.git = executable(args.git_bin, "Git binary")
        self.git_lfs = executable(args.git_lfs_bin, "Git LFS binary")
        self.remote_url = redact_url(args.remote_url)
        self.evidence_dir = args.evidence_dir.resolve()
        self.repo = (args.repo_dir or self.evidence_dir / "source").resolve()
        if self.evidence_dir.exists():
            raise QualificationError(f"evidence directory already exists: {self.evidence_dir}")
        if self.repo.exists():
            raise QualificationError(f"repository directory already exists: {self.repo}")
        if args.object_count < args.path_count:
            raise QualificationError("object-count must be at least path-count")
        if args.commit_count < 1 or args.path_count < 1 or args.object_count < 1:
            raise QualificationError("commit-count, path-count, and object-count must be positive")
        if args.min_size < 0 or args.max_size < args.min_size:
            raise QualificationError("size range is invalid")
        self.secrets = tuple(
            value
            for key, value in os.environ.items()
            if key in SECRET_ENV_KEYS and value
        ) + tuple(
            value
            for value in (urlsplit(args.remote_url).username, urlsplit(args.remote_url).password)
            if value
        )
        self.env = self._build_env()
        self.final_objects: dict[str, dict[str, Any]] = {}
        self.report: dict[str, Any] = {
            "schema": SCHEMA,
            "version": VERSION,
            "status": "running",
            "started_at": utc_now(),
            "finished_at": None,
            "profile": args.profile,
            "seed": args.seed,
            "remote_url": self.remote_url,
            "repository": str(self.repo),
            "evidence_dir": str(self.evidence_dir),
            "workload": {
                "object_count": args.object_count,
                "commit_count": args.commit_count,
                "path_count": args.path_count,
                "min_size": args.min_size,
                "max_size": args.max_size,
            },
            "versions": {},
            "commands": [],
            "checks": [],
            "objects": [],
            "metrics": {},
            "error": None,
        }

    def _build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "GIT_TERMINAL_PROMPT": "0",
                "GIT_AUTHOR_NAME": "Crab LFS qualification",
                "GIT_AUTHOR_EMAIL": "crab-lfs-qualification@example.invalid",
                "GIT_COMMITTER_NAME": "Crab LFS qualification",
                "GIT_COMMITTER_EMAIL": "crab-lfs-qualification@example.invalid",
                "GIT_LFS_SKIP_SMUDGE": "1",
            }
        )
        return env

    def save(self) -> None:
        self.evidence_dir.mkdir(parents=True, exist_ok=True)
        target = self.evidence_dir / "report.json"
        temporary = target.with_suffix(".json.tmp")
        temporary.write_text(
            json.dumps(self.report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        temporary.replace(target)

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        self.report["checks"].append({"name": name, "ok": ok, "detail": detail or {}})
        self.save()
        if not ok:
            raise QualificationError(f"check failed: {name}")

    def run(self, name: str, command: Sequence[str], cwd: Path, *, check: bool = True) -> str:
        started = time.monotonic()
        completed = subprocess.run(
            list(command),
            cwd=cwd,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        duration_ms = int((time.monotonic() - started) * 1000)
        stdout = redact_text(completed.stdout, self.secrets)
        stderr = redact_text(completed.stderr, self.secrets)
        self.report["commands"].append(
            {
                "name": name,
                "argv": [redact_text(str(item), self.secrets) for item in command],
                "cwd": str(cwd),
                "exit_code": completed.returncode,
                "duration_ms": duration_ms,
                "stdout": stdout[-16_384:],
                "stderr": stderr[-16_384:],
            }
        )
        self.save()
        if check and completed.returncode != 0:
            raise QualificationError(f"{name} failed with exit code {completed.returncode}")
        return stdout

    def run_git(self, args: Sequence[str], cwd: Path, name: str) -> str:
        return self.run(name, [self.git, *args], cwd)

    def run_lfs(self, args: Sequence[str], cwd: Path, name: str) -> str:
        return self.run(name, [self.git_lfs, *args], cwd)

    def run_crab(self, args: Sequence[str], cwd: Path, name: str) -> str:
        return self.run(name, [self.crab, *args], cwd)

    def setup(self) -> None:
        self.evidence_dir.mkdir(parents=True)
        helper = self.evidence_dir / "git-remote-crab"
        try:
            helper.symlink_to(self.crab)
        except OSError:
            shutil.copy2(self.crab, helper)
        self.env["PATH"] = str(self.evidence_dir) + os.pathsep + self.env.get("PATH", "")
        self.repo.mkdir(parents=True)
        self.run_git(["init", "--initial-branch=main"], self.repo, "git init")
        self.run_git(["config", "user.name", "Crab LFS qualification"], self.repo, "git identity name")
        self.run_git(["config", "user.email", "crab-lfs-qualification@example.invalid"], self.repo, "git identity email")
        self.run_crab(
            [*self.args.crab_args, "init", self.args.remote_url],
            self.repo,
            "crab init",
        )
        self.run_crab(
            [*self.args.crab_args, "lfs", "install", "--local", "--skip-repo"],
            self.repo,
            "crab lfs install",
        )
        self.run_lfs(["track", "*.bin"], self.repo, "git lfs track")
        self.run_git(["add", ".gitattributes", ".crab.toml"], self.repo, "stage LFS configuration")

        self.report["versions"] = {
            "git": self.run_git(["--version"], self.repo, "git version").strip(),
            "git_lfs": self.run_lfs(["version"], self.repo, "git lfs version").strip(),
            "crab": self.run_crab([*self.args.crab_args, "version"], self.repo, "crab version").strip(),
            "python": platform.python_version(),
        }

    def create_history(self) -> None:
        sizes: list[int] = []
        for index in range(self.args.object_count):
            span = self.args.max_size - self.args.min_size
            size = self.args.min_size + ((index * 1_000_003) % (span + 1)) if span else self.args.min_size
            path_index = index % self.args.path_count
            path = self.repo / f"asset-{path_index:05d}.bin"
            oid = write_deterministic(path, size, self.args.seed, index)
            self.report["objects"].append({"index": index, "path": path.name, "size": size, "oid": oid})
            self.final_objects[path.name] = self.report["objects"][-1]
            sizes.append(size)
            if (index + 1) % max(1, self.args.object_count // 10) == 0:
                self.save()

            should_commit = (
                index == self.args.object_count - 1
                or (index + 1) * self.args.commit_count // self.args.object_count
                > index * self.args.commit_count // self.args.object_count
            )
            if should_commit:
                commit_index = (index + 1) * self.args.commit_count // self.args.object_count
                self.run_git(["add", "*.bin"], self.repo, f"stage objects {index + 1}")
                self.run_git(["add", ".gitattributes", ".crab.toml"], self.repo, f"stage config {index + 1}")
                self.run_git(["commit", "-m", f"qualification commit {commit_index}"], self.repo, f"commit {commit_index}")
        self.report["metrics"]["logical_bytes"] = sum(sizes)
        self.report["metrics"]["current_paths"] = len(list(self.repo.glob("*.bin")))
        self.save()

    def verify_source(self) -> None:
        self.run_lfs(["push", "origin", "main"], self.repo, "git lfs push")
        self.run_git(["push", "origin", "main"], self.repo, "git push")
        local_ref = self.run_git(["rev-parse", "refs/heads/main"], self.repo, "local ref").strip()
        remote_refs = self.run_git(["ls-remote", self.args.remote_url, "refs/heads/main"], self.repo, "remote ref")
        remote_ref = remote_refs.split()[0] if remote_refs.split() else ""
        self.check("ref-equality", local_ref == remote_ref, {"local": local_ref, "remote": remote_ref})
        self.run_lfs(["fsck"], self.repo, "git lfs fsck")
        self.run_crab([*self.args.crab_args, "lfs", "fsck"], self.repo, "crab lfs fsck")

    def verify_clone(self) -> None:
        clone = self.evidence_dir / "clone"
        self.run_git(["clone", self.args.remote_url, str(clone)], self.evidence_dir, "skip-smudge clone")
        self.run_crab(
            [*self.args.crab_args, "lfs", "install", "--local", "--skip-smudge", "--skip-repo"],
            clone,
            "clone LFS install",
        )
        self.run_lfs(["fetch", "origin", "main"], clone, "git lfs fetch")
        self.run_lfs(["checkout"], clone, "git lfs checkout")
        self.run_lfs(["fsck"], clone, "cloned git lfs fsck")
        self.run_crab([*self.args.crab_args, "lfs", "fsck"], clone, "cloned crab lfs fsck")
        for item in self.final_objects.values():
            path = clone / item["path"]
            self.check(
                f"byte-identity-{item['index']}",
                path.is_file() and path.stat().st_size == item["size"] and hash_file(path) == item["oid"],
                {"path": item["path"], "size": item["size"]},
            )

    def run_all(self) -> None:
        started_children = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        self.setup()
        self.create_history()
        self.verify_source()
        self.verify_clone()
        ended_children = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        # macOS reports ru_maxrss in bytes; Linux and most other Unix
        # platforms report it in KiB. Keep the qualification report honest
        # on the developer platform as well as in CI.
        rss_unit = 1 if sys.platform == "darwin" else 1024
        self.report["metrics"]["peak_child_rss_bytes"] = max(started_children, ended_children) * rss_unit
        self.report["metrics"]["object_count"] = len(self.report["objects"])
        self.report["metrics"]["wire_bytes"] = None
        self.report["metrics"]["wire_bytes_source"] = "not instrumented by the standalone client"
        self.report["status"] = "passed"
        self.report["finished_at"] = utc_now()
        self.save()


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crab-bin", default="crab")
    parser.add_argument("--git-bin", default="git")
    parser.add_argument("--git-lfs-bin", default="git-lfs")
    parser.add_argument("--remote-url", required=True)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--repo-dir", type=Path)
    parser.add_argument("--profile", default="smoke", choices=["smoke", "scale-safe", "full-scale", "large-object"])
    parser.add_argument("--object-count", type=int, default=10)
    parser.add_argument("--commit-count", type=int, default=3)
    parser.add_argument("--path-count", type=int, default=10)
    parser.add_argument("--min-size", type=int, default=DEFAULT_MIN_SIZE)
    parser.add_argument("--max-size", type=int, default=DEFAULT_MAX_SIZE)
    parser.add_argument("--seed", default="crab-lfs-smoke")
    parser.add_argument("--crab-arg", dest="crab_args", action="append", default=[])
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    qualification: Qualification | None = None
    try:
        qualification = Qualification(args)
        qualification.run_all()
        print(json.dumps({"status": "passed", "report": str(qualification.evidence_dir / "report.json")}))
        return 0
    except (QualificationError, OSError, subprocess.SubprocessError) as error:
        if qualification is not None:
            qualification.report["status"] = "failed"
            qualification.report["error"] = redact_text(str(error), qualification.secrets)
            qualification.report["finished_at"] = utc_now()
            qualification.save()
        print(f"qualification failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
