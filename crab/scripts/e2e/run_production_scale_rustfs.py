#!/usr/bin/env python3
"""Verify a production-shaped 200 GiB Crab repository against local RustFS.

The qualification is deliberately explicit. It creates one isolated run
beneath ``/Volumes/Workspace/CrabRepos/ml-model-200gb`` and keeps its source, clone,
logs, manifests, and report for investigation. The files are sparse with
deterministic repeated blocks: they exercise 200 GiB of logical content and
deduplication without consuming several additional terabytes of host storage.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


MIB = 1024 * 1024
GIB = 1024 * MIB
PATCH_BLOCK_BYTES = MIB
DEFAULT_ROOT = Path("/Volumes/Workspace/CrabRepos")
DEFAULT_BUCKET = "crab"
DEFAULT_ENDPOINT = "http://127.0.0.1:9000"
DEFAULT_VERSION_COUNT = 12
DEFAULT_VERSION_PATCH_BYTES = 16 * MIB
DEFAULT_RUN_NAME = "ml-model-200gb"
RUN_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
SECRET_ENV_KEYS = {"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"}
SCRIPT_DIR = Path(__file__).resolve().parent
CRAB_DIR = SCRIPT_DIR.parents[1]
REPO_ROOT = SCRIPT_DIR.parents[2]
START_RUSTFS = CRAB_DIR / "scripts" / "start-rustfs.sh"


class WorkflowError(RuntimeError):
    """Raised when one production-scale verification check fails."""


@dataclass(frozen=True)
class FileSpec:
    path: str
    size: int
    family: str
    seed: int


@dataclass(frozen=True)
class WorkloadPlan:
    files: tuple[FileSpec, ...]
    logical_bytes: int
    version_target: FileSpec
    version_count: int
    version_patch_bytes: int


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
class Report:
    run_id: str
    status: str
    root: str
    source: str
    clone: str
    remote_url: str
    endpoint_url: str
    logical_bytes: int
    files: int
    version_count: int
    env: dict[str, str]
    commands: list[dict[str, Any]] = field(default_factory=list)
    checks: list[dict[str, Any]] = field(default_factory=list)
    versions: list[dict[str, Any]] = field(default_factory=list)
    artifacts: dict[str, str] = field(default_factory=dict)
    started_at: str = ""
    finished_at: str = ""
    error: str | None = None


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def make_run_id() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def safe_run_id(value: str) -> str:
    if not RUN_ID_RE.fullmatch(value):
        raise WorkflowError("run id may contain only letters, numbers, dot, underscore, or dash")
    return value


def slug(value: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9_.-]+", "-", value.lower()).strip("-")
    return cleaned or "command"


def block(label: str, length: int) -> bytes:
    digest = hashlib.blake2b(label.encode("utf-8"), digest_size=32).digest()
    return (digest * ((length + len(digest) - 1) // len(digest)))[:length]


def build_workload_plan() -> WorkloadPlan:
    files: list[FileSpec] = []
    foundation = FileSpec("models/foundation-model-000.safetensors", 5 * GIB, "model", 0x1000)
    files.append(foundation)
    files.extend(
        FileSpec(f"models/checkpoint-{index:03}.safetensors", 5 * GIB, "model", 0x1001 + index)
        for index in range(9)
    )
    files.extend(
        FileSpec(f"datasets/train-shard-{index:03}.parquet", GIB, "dataset", 0x2000 + index)
        for index in range(40)
    )
    files.extend(
        FileSpec(f"embeddings/partition-{index:03}.arrow", 512 * MIB, "embedding", 0x3000 + index)
        for index in range(100)
    )
    files.extend(
        FileSpec(f"cache/tensor-{index:03}.bin", 256 * MIB, "tensor", 0x4000 + index)
        for index in range(200)
    )
    files.extend(
        FileSpec(f"metadata/batch-{index:03}.jsonl", 50 * MIB, "metadata", 0x5000 + index)
        for index in range(200)
    )
    files.append(FileSpec("checkpoints/catalog.ckpt", 240 * MIB, "checkpoint", 0x6000))
    logical_bytes = sum(item.size for item in files)
    return WorkloadPlan(
        files=tuple(files),
        logical_bytes=logical_bytes,
        version_target=foundation,
        version_count=DEFAULT_VERSION_COUNT,
        version_patch_bytes=DEFAULT_VERSION_PATCH_BYTES,
    )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(8 * MIB):
            digest.update(chunk)
    return digest.hexdigest()


def manifest_total_bytes(manifest: dict[str, Any]) -> int:
    return sum(int(item["size"]) for item in manifest["files"])


class ProductionScaleRunner:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.plan = build_workload_plan()
        self.run_id = safe_run_id(args.run_id or make_run_id())
        self.run_root = args.root / DEFAULT_RUN_NAME / self.run_id
        self.source = self.run_root / "source"
        self.clone = self.run_root / "clone"
        self.logs = self.run_root / "logs"
        self.artifacts = self.run_root / "artifacts"
        self.cache = self.run_root / "cache"
        self.remote_url = f"crab://{args.bucket}/e2e-production-scale/{self.run_id}"
        self.command_index = 0
        self.env = self.build_env()
        self.report = Report(
            run_id=self.run_id,
            status="running",
            root=str(self.run_root),
            source=str(self.source),
            clone=str(self.clone),
            remote_url=self.remote_url,
            endpoint_url=args.endpoint_url,
            logical_bytes=self.plan.logical_bytes,
            files=len(self.plan.files),
            version_count=self.plan.version_count,
            env=self.redacted_env(),
            started_at=utc_now(),
        )

    def build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "AWS_ACCESS_KEY_ID": "crab",
                "AWS_SECRET_ACCESS_KEY": "crab",
                "AWS_REGION": "us-east-1",
                "AWS_DEFAULT_REGION": "us-east-1",
                "AWS_ENDPOINT_URL": self.args.endpoint_url,
                "AWS_ENDPOINT_URL_S3": self.args.endpoint_url,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "CRAB_CACHE_DIR": str(self.cache),
                "GIT_TERMINAL_PROMPT": "0",
                "GIT_MERGE_AUTOEDIT": "no",
            }
        )
        return env

    def redacted_env(self) -> dict[str, str]:
        result: dict[str, str] = {}
        for key, value in sorted(self.env.items()):
            if key in SECRET_ENV_KEYS:
                result[key] = "<redacted>"
            elif key.startswith(("AWS_", "CRAB_", "GIT_", "VIRTUAL_")):
                result[key] = value
        return result

    def write_report(self) -> None:
        self.artifacts.mkdir(parents=True, exist_ok=True)
        report_path = self.artifacts / "report.json"
        self.report.artifacts["report"] = str(report_path)
        report_path.write_text(json.dumps(asdict(self.report), indent=2, sort_keys=True) + "\n")

    def check(self, name: str, ok: bool, detail: dict[str, Any]) -> None:
        self.report.checks.append({"name": name, "ok": ok, "detail": detail, "checked_at": utc_now()})
        self.write_report()
        if not ok:
            raise WorkflowError(f"check failed: {name}")

    def run_cmd(
        self,
        name: str,
        args: list[str],
        *,
        cwd: Path,
        check: bool = True,
        timeout: int | None = None,
    ) -> CommandRecord:
        self.command_index += 1
        base = f"{self.command_index:03d}-{slug(name)}"
        self.logs.mkdir(parents=True, exist_ok=True)
        stdout_log = self.logs / f"{base}.stdout.log"
        stderr_log = self.logs / f"{base}.stderr.log"
        started = time.monotonic()
        try:
            with stdout_log.open("wb") as stdout_handle, stderr_log.open("wb") as stderr_handle:
                result = subprocess.run(
                    args,
                    cwd=cwd,
                    env=self.env,
                    stdout=stdout_handle,
                    stderr=stderr_handle,
                    timeout=timeout,
                    check=False,
                )
            exit_code = result.returncode
        except subprocess.TimeoutExpired:
            exit_code = 124
            with stderr_log.open("ab") as stderr_handle:
                stderr_handle.write(f"timed out after {timeout} seconds\n".encode("utf-8"))
        record = CommandRecord(
            name=name,
            args=args,
            cwd=str(cwd),
            exit_code=exit_code,
            duration_ms=round((time.monotonic() - started) * 1000),
            stdout_log=str(stdout_log),
            stderr_log=str(stderr_log),
        )
        self.report.commands.append(asdict(record))
        self.write_report()
        if check and exit_code != 0:
            raise WorkflowError(f"{name} failed; stdout={stdout_log} stderr={stderr_log}")
        return record

    def run_git(self, repo: Path, args: list[str], name: str) -> CommandRecord:
        return self.run_cmd(name, ["git", *args], cwd=repo)

    def run_crab(self, repo: Path, args: list[str], name: str, *, timeout: int | None = None) -> CommandRecord:
        return self.run_cmd(name, ["crab", *args], cwd=repo, timeout=timeout)

    def stdout_json(self, record: CommandRecord) -> dict[str, Any]:
        try:
            return json.loads(Path(record.stdout_log).read_text())
        except json.JSONDecodeError as exc:
            raise WorkflowError(f"{record.name} did not emit JSON: {record.stdout_log}") from exc

    def setup(self) -> None:
        if self.run_root.exists():
            raise WorkflowError(f"run root already exists: {self.run_root}")
        self.logs.mkdir(parents=True, exist_ok=False)
        self.cache.mkdir(parents=True, exist_ok=True)
        self.write_report()

    def preflight(self) -> None:
        required = 550 * GIB
        free = shutil.disk_usage(self.args.root).free
        self.check(
            "workspace-capacity",
            free >= required,
            {"free_bytes": free, "required_bytes": required, "logical_bytes": self.plan.logical_bytes},
        )
        required_commands = ["aws", "bash", "git", "python3"]
        missing = [command for command in required_commands if shutil.which(command) is None]
        self.check("host-dependencies", not missing, {"missing": missing})
        self.check("rustfs-launcher", START_RUSTFS.is_file(), {"path": str(START_RUSTFS)})

    def start_rustfs(self) -> None:
        self.run_cmd("start RustFS", ["bash", str(START_RUSTFS)], cwd=REPO_ROOT, timeout=10 * 60)
        self.run_cmd(
            "RustFS bucket readiness",
            ["aws", "--endpoint-url", self.args.endpoint_url, "s3api", "head-bucket", "--bucket", self.args.bucket],
            cwd=self.run_root,
        )
        self.check("rustfs-bucket-ready", True, {"endpoint": self.args.endpoint_url, "bucket": self.args.bucket})

    def install_crab(self) -> None:
        if self.args.skip_install:
            self.check("make-install", True, {"skipped": True})
            return
        self.run_cmd("make install", ["make", "-C", str(CRAB_DIR), "install"], cwd=REPO_ROOT, timeout=60 * 60)
        self.run_cmd("crab version", ["crab", "version"], cwd=self.run_root)
        self.check("make-install", True, {"crab_dir": str(CRAB_DIR)})

    def init_source(self) -> None:
        self.source.mkdir(parents=True)
        self.run_git(self.source, ["init", "-b", "main"], "git init source")
        self.run_git(self.source, ["config", "user.email", "production-scale@crab.local"], "git config email")
        self.run_git(self.source, ["config", "user.name", "Crab Production Scale"], "git config name")
        self.run_git(self.source, ["config", "commit.gpgsign", "false"], "git disable commit signing")
        (self.source / ".gitignore").write_text("._*\n**/._*\n.DS_Store\n", encoding="utf-8")
        self.run_crab(
            self.source, ["init", self.remote_url], "crab init source"
        )
        for pattern in ("*.safetensors", "*.parquet", "*.arrow", "*.bin", "*.jsonl", "*.ckpt"):
            self.run_crab(self.source, ["track", pattern], f"crab track {pattern}")

    def write_sparse_file(self, root: Path, spec: FileSpec, *, version: int = 1) -> None:
        path = root / spec.path
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("wb") as handle:
            handle.truncate(spec.size)
        offsets = (0, PATCH_BLOCK_BYTES, spec.size // 3, (spec.size * 2) // 3)
        labels = (
            "crab-production-scale:shared-header",
            f"crab-production-scale:family:{spec.family}",
            f"crab-production-scale:family-version:{spec.family}:{version}",
            f"crab-production-scale:asset:{spec.seed}",
        )
        with path.open("r+b", buffering=0) as handle:
            for offset, label in zip(offsets, labels, strict=True):
                handle.seek(min(offset, spec.size - PATCH_BLOCK_BYTES))
                handle.write(block(label, PATCH_BLOCK_BYTES))

    def create_manifest(self, root: Path, specs: list[FileSpec], name: str) -> dict[str, Any]:
        entries: list[dict[str, Any]] = []
        for index, spec in enumerate(specs, start=1):
            path = root / spec.path
            entries.append(
                {
                    "path": spec.path,
                    "size": path.stat().st_size,
                    "sha256": sha256_file(path),
                    "family": spec.family,
                    "version": 1,
                }
            )
            if index % 25 == 0 or index == len(specs):
                print(f"hashed {index}/{len(specs)} files for {name}", flush=True)
        manifest = {"schema": "crab.production-scale-manifest", "name": name, "files": entries, "total_bytes": sum(item["size"] for item in entries)}
        path = self.artifacts / f"{name}.manifest.json"
        path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        self.report.artifacts[f"{name}_manifest"] = str(path)
        self.write_report()
        return manifest

    def create_workload(self) -> dict[str, Any]:
        print(f"creating {self.plan.logical_bytes / GIB:.0f} GiB logical workload", flush=True)
        for index, spec in enumerate(self.plan.files, start=1):
            self.write_sparse_file(self.source, spec)
            if index % 50 == 0 or index == len(self.plan.files):
                print(f"created {index}/{len(self.plan.files)} sparse files", flush=True)
        manifest = self.create_manifest(self.source, list(self.plan.files), "initial")
        physical_bytes = sum((self.source / item["path"]).stat().st_blocks * 512 for item in manifest["files"])
        self.check(
            "logical-200-gib-workload",
            manifest_total_bytes(manifest) == 200 * GIB,
            {"files": len(manifest["files"]), "logical_bytes": manifest_total_bytes(manifest), "physical_bytes": physical_bytes},
        )
        return manifest

    def stage_paths(self, repo: Path, paths: list[str], name: str) -> None:
        self.run_crab(repo, ["add", *paths, "--jobs", str(self.args.jobs), "--jsonl"], f"{name} crab add", timeout=12 * 60 * 60)
        self.run_git(repo, ["add", "--", ".crab.toml", ".gitattributes", ".gitignore"], f"{name} git add metadata")
        self.run_git(repo, ["add", "-A", "--", *paths], f"{name} git add pointers")

    def commit(self, repo: Path, message: str) -> None:
        self.run_git(repo, ["commit", "-m", message], f"git commit {message}")

    def push(self, repo: Path, name: str) -> CommandRecord:
        last: CommandRecord | None = None
        for attempt in range(1, 4):
            last = self.run_cmd(
                f"{name} push attempt {attempt}",
                ["crab", "push", "--jsonl", "--upload-concurrency", str(self.args.upload_concurrency)],
                cwd=repo,
                check=False,
                timeout=12 * 60 * 60,
            )
            if last.exit_code == 0:
                return last
            time.sleep(attempt * 3)
        raise WorkflowError(f"{name} push failed after retries; stderr={last.stderr_log if last else '<none>'}")

    def pointer_check(self, repo: Path, manifest: dict[str, Any], name: str) -> None:
        failures: list[str] = []
        for item in manifest["files"]:
            path = item["path"]
            result = subprocess.run(["git", "show", f":{path}"], cwd=repo, env=self.env, capture_output=True, check=False)
            blob = result.stdout
            if result.returncode != 0 or len(blob) > 256 or not blob.startswith(b"version https://crab.dev/spec/v1\n"):
                failures.append(path)
                continue
            if f"size {item['size']}\n".encode("utf-8") not in blob:
                failures.append(path)
        self.check(name, not failures, {"files": len(manifest["files"]), "failures": failures[:10]})

    def object_stats(self, prefix: str, name: str) -> dict[str, int]:
        count = 0
        total = 0
        token: str | None = None
        page = 0
        while True:
            args = [
                "aws",
                "--no-paginate",
                "--endpoint-url",
                self.args.endpoint_url,
                "s3api",
                "list-objects-v2",
                "--bucket",
                self.args.bucket,
                "--prefix",
                prefix,
                "--max-keys",
                "1000",
            ]
            if token:
                args.extend(["--continuation-token", token])
            record = self.run_cmd(f"{name} object page {page}", args, cwd=self.run_root)
            payload = self.stdout_json(record)
            entries = payload.get("Contents", [])
            count += len(entries)
            total += sum(int(item.get("Size", 0)) for item in entries)
            if not payload.get("IsTruncated"):
                break
            token = payload.get("NextContinuationToken")
            if not token:
                raise WorkflowError(f"{name} object listing was truncated without a continuation token")
            page += 1
        return {"objects": count, "bytes": total}

    def run_initial_push(self, manifest: dict[str, Any]) -> None:
        before = self.object_stats(".crab/xorbs/", "baseline xorb")
        paths = [item["path"] for item in manifest["files"]]
        self.stage_paths(self.source, paths, "initial")
        self.commit(self.source, "production-scale initial 200 GiB repository")
        self.push(self.source, "initial")
        self.pointer_check(self.source, manifest, "initial-index-pointers")
        after = self.object_stats(".crab/xorbs/", "initial xorb")
        delta = after["bytes"] - before["bytes"]
        self.check(
            "initial-cross-file-dedup-storage",
            0 <= delta < 5 * GIB,
            {"before": before, "after": after, "xorb_delta_bytes": delta, "logical_bytes": manifest["total_bytes"]},
        )

    def find_entry(self, manifest: dict[str, Any], path: str) -> dict[str, Any]:
        for item in manifest["files"]:
            if item["path"] == path:
                return item
        raise WorkflowError(f"manifest is missing {path}")

    def write_version_patch(self, version: int) -> int:
        target = self.source / self.plan.version_target.path
        maximum = target.stat().st_size - self.plan.version_patch_bytes
        offset = (version * 389 * MIB) % maximum
        with target.open("r+b", buffering=0) as handle:
            handle.seek(offset)
            handle.write(block(f"crab-production-scale:version:{version}", self.plan.version_patch_bytes))
        return offset

    def diff_report(self) -> dict[str, Any]:
        record = self.run_crab(self.source, ["diff", "--json", "HEAD~1", "HEAD"], "crab diff version")
        payload = self.stdout_json(record)
        try:
            reports = [item["report"] for item in payload["data"]["files"]]
        except (KeyError, TypeError) as exc:
            raise WorkflowError(f"unexpected crab diff JSON: {record.stdout_log}") from exc
        for report in reports:
            if report.get("path") == self.plan.version_target.path:
                return report
        raise WorkflowError("crab diff did not report the versioned foundation model")

    def run_versions(self, manifest: dict[str, Any]) -> dict[str, Any]:
        target_path = self.plan.version_target.path
        for version in range(2, self.plan.version_count + 2):
            before = self.object_stats(".crab/xorbs/", f"version {version} baseline xorb")
            offset = self.write_version_patch(version)
            entry = self.find_entry(manifest, target_path)
            entry["sha256"] = sha256_file(self.source / target_path)
            entry["version"] = version
            self.stage_paths(self.source, [target_path], f"version {version}")
            self.commit(self.source, f"production-scale foundation model version {version}")
            self.push(self.source, f"version {version}")
            diff = self.diff_report()
            after = self.object_stats(".crab/xorbs/", f"version {version} xorb")
            xorb_delta = after["bytes"] - before["bytes"]
            unchanged = int(diff.get("unchanged_bytes", 0))
            dedup_ratio = float(diff.get("dedup_ratio", 0))
            version_ok = (
                unchanged >= self.plan.version_target.size - self.plan.version_patch_bytes
                and dedup_ratio > 0.90
                and 0 <= xorb_delta <= 256 * MIB
            )
            detail = {
                "version": version,
                "offset": offset,
                "patch_bytes": self.plan.version_patch_bytes,
                "unchanged_bytes": unchanged,
                "dedup_ratio": dedup_ratio,
                "xorb_delta_bytes": xorb_delta,
                "before": before,
                "after": after,
            }
            self.report.versions.append(detail)
            self.check(f"version-{version}-delta-dedup", version_ok, detail)
        manifest["name"] = "versioned"
        manifest["total_bytes"] = manifest_total_bytes(manifest)
        path = self.artifacts / "versioned.manifest.json"
        path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        self.report.artifacts["versioned_manifest"] = str(path)
        self.write_report()
        return manifest

    def verify_manifest(self, repo: Path, manifest: dict[str, Any], name: str) -> None:
        failures: list[dict[str, Any]] = []
        for index, item in enumerate(manifest["files"], start=1):
            path = repo / item["path"]
            if not path.is_file():
                failures.append({"path": item["path"], "reason": "missing"})
                continue
            size = path.stat().st_size
            digest = sha256_file(path)
            if size != item["size"] or digest != item["sha256"]:
                failures.append({"path": item["path"], "size": size, "sha256": digest})
            if index % 25 == 0 or index == len(manifest["files"]):
                print(f"verified {index}/{len(manifest['files'])} files for {name}", flush=True)
        self.check(name, not failures, {"files": len(manifest["files"]), "failures": failures[:5]})

    def worktree_pointers(self, repo: Path, manifest: dict[str, Any], name: str) -> None:
        failures: list[str] = []
        for item in manifest["files"]:
            path = repo / item["path"]
            if not path.is_file() or path.stat().st_size > 256:
                failures.append(item["path"])
                continue
            if not path.read_bytes().startswith(b"version https://crab.dev/spec/v1\n"):
                failures.append(item["path"])
        self.check(name, not failures, {"files": len(manifest["files"]), "failures": failures[:10]})

    def clone_hydrate_cycle(self, manifest: dict[str, Any]) -> None:
        self.run_cmd("crab clone lazy", ["crab", "clone", self.remote_url, str(self.clone), "--jsonl"], cwd=self.run_root, timeout=12 * 60 * 60)
        self.run_git(self.clone, ["config", "user.email", "clone-developer@crab.local"], "clone git config email")
        self.run_git(self.clone, ["config", "user.name", "Clone Developer"], "clone git config name")
        self.worktree_pointers(self.clone, manifest, "lazy-clone-pointers")
        target = self.plan.version_target.path
        self.run_crab(self.clone, ["hydrate", target, "--jsonl"], "clone selective hydrate", timeout=12 * 60 * 60)
        target_entry = self.find_entry(manifest, target)
        target_ok = sha256_file(self.clone / target) == target_entry["sha256"]
        self.check("selective-5-gib-hydrate", target_ok, {"path": target, "size": target_entry["size"]})
        self.run_crab(self.clone, ["hydrate", "--all", "--jsonl"], "clone hydrate all", timeout=12 * 60 * 60)
        self.verify_manifest(self.clone, manifest, "clone-hydrate-byte-identity")
        self.run_crab(self.clone, ["dehydrate", "--all", "--jsonl"], "clone dehydrate all", timeout=12 * 60 * 60)
        self.worktree_pointers(self.clone, manifest, "clone-dehydrate-pointers")
        self.run_crab(self.clone, ["hydrate", "--all", "--jsonl"], "clone rehydrate all", timeout=12 * 60 * 60)
        self.verify_manifest(self.clone, manifest, "clone-rehydrate-byte-identity")

    def reverse_push(self, manifest: dict[str, Any]) -> dict[str, Any]:
        spec = FileSpec("experiments/retrained-adapter.bin", 256 * MIB, "adapter", 0x7000)
        self.write_sparse_file(self.clone, spec)
        digest = sha256_file(self.clone / spec.path)
        self.stage_paths(self.clone, [spec.path], "clone reverse change")
        self.commit(self.clone, "production-scale clone developer adapter")
        self.push(self.clone, "clone reverse change")
        manifest["files"].append({"path": spec.path, "size": spec.size, "sha256": digest, "family": spec.family, "version": 1})
        manifest["files"].sort(key=lambda item: item["path"])
        manifest["name"] = "clone-reverse-push"
        manifest["total_bytes"] = manifest_total_bytes(manifest)

        self.run_crab(self.source, ["dehydrate", "--all", "--jsonl"], "source dehydrate before reverse pull", timeout=12 * 60 * 60)
        self.run_git(self.source, ["add", "-u"], "source refresh dehydrated index")
        self.run_crab(
            self.source,
            ["pull", "--remote", "origin", "--branch", "main", "--no-hydrate", "--jsonl"],
            "source pull clone change",
            timeout=12 * 60 * 60,
        )
        self.run_crab(self.source, ["hydrate", "--all", "--jsonl"], "source hydrate clone change", timeout=12 * 60 * 60)
        self.verify_manifest(self.source, manifest, "source-after-clone-reverse-push")
        path = self.artifacts / "final.manifest.json"
        path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        self.report.artifacts["final_manifest"] = str(path)
        self.write_report()
        return manifest

    def run_health_checks(self) -> None:
        for repo, label in ((self.source, "source"), (self.clone, "clone")):
            self.run_crab(repo, ["fsck"], f"{label} crab fsck", timeout=12 * 60 * 60)
            self.run_git(repo, ["fsck"], f"{label} git fsck")
        remote = self.object_stats(f"e2e-production-scale/{self.run_id}/", "repo remote")
        self.check("remote-repository-objects", remote["objects"] > 0, remote)

    def run(self) -> int:
        self.setup()
        try:
            self.preflight()
            self.start_rustfs()
            self.install_crab()
            self.init_source()
            manifest = self.create_workload()
            self.run_initial_push(manifest)
            manifest = self.run_versions(manifest)
            self.clone_hydrate_cycle(manifest)
            self.reverse_push(manifest)
            self.run_health_checks()
            self.report.status = "ok"
            return 0
        except Exception as exc:
            self.report.status = "failed"
            self.report.error = str(exc)
            print(f"error: {exc}", file=sys.stderr)
            return 1
        finally:
            self.report.finished_at = utc_now()
            self.write_report()
            print(f"Run ID: {self.run_id}")
            print(f"Remote: {self.remote_url}")
            print(f"Report: {self.artifacts / 'report.json'}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--bucket", default=DEFAULT_BUCKET)
    parser.add_argument("--endpoint-url", default=DEFAULT_ENDPOINT)
    parser.add_argument("--run-id")
    parser.add_argument("--jobs", type=int, default=8)
    parser.add_argument("--upload-concurrency", type=int, default=16)
    parser.add_argument("--skip-install", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        return ProductionScaleRunner(args).run()
    except WorkflowError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
