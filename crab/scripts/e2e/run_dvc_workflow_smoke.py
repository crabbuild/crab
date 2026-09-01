#!/usr/bin/env python3
"""Run a DVC-style Crab workflow/experiment smoke against RustFS.

The script creates a unique Crab repo under ``crab://<bucket>/e2e-dvc/<run-id>``
and keeps all local artifacts under ``~/Workspace/CrabRepos`` by
default. It is intentionally command-level: every check shells out to the
installed ``crab`` binary rather than importing Rust code or test helpers.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DEFAULT_ROOT = Path.home() / "Workspace" / "CrabRepos"
DEFAULT_BUCKET = "crab"
DEFAULT_ENDPOINT = "http://127.0.0.1:9000"
REMOTE_PREFIX = "e2e-dvc"
SECRET_KEYS = {"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"}


class SmokeError(RuntimeError):
    """Raised when a smoke step fails."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def make_run_id() -> str:
    return "dvc-e2e-" + datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def slug(value: str) -> str:
    out = "".join(c if c.isalnum() or c in "._-" else "-" for c in value.lower())
    return out.strip("-") or "command"


def redact_env(env: dict[str, str]) -> dict[str, str]:
    redacted = {}
    for key, value in sorted(env.items()):
        if key in SECRET_KEYS:
            redacted[key] = "<redacted>"
        elif key.startswith("AWS_") or key.startswith("CRAB_"):
            redacted[key] = value
    return redacted


def params_yaml(scale: float, bias: float) -> str:
    return f"model:\n  scale: {scale}\n  bias: {bias}\n"


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
class SmokeReport:
    run_id: str
    status: str
    remote: str
    root: str
    endpoint_url: str
    source_sha: str
    workflow_run_id: str
    workflow_run_attempt: str
    crab_version: str
    platform: str
    rustfs_image: str
    env: dict[str, str]
    commands: list[dict[str, Any]] = field(default_factory=list)
    checks: list[dict[str, Any]] = field(default_factory=list)
    artifacts: dict[str, str] = field(default_factory=dict)
    updated_at: str = field(default_factory=utc_now)


class Smoke:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.run_id = args.run_id or make_run_id()
        self.root = args.root
        self.run_root = self.root / self.run_id
        self.source = self.run_root / "source"
        self.clone = self.run_root / "clone"
        self.hydra = self.run_root / "hydra"
        self.artifacts = self.run_root / "artifacts"
        self.logs = self.run_root / "logs"
        self.remote = f"crab://{args.bucket}/{REMOTE_PREFIX}/{self.run_id}"
        self.hydra_remote = f"crab://{args.bucket}/{REMOTE_PREFIX}/{self.run_id}-hydra"
        self.env = self.build_env()
        self.command_index = 0
        self.report = SmokeReport(
            run_id=self.run_id,
            status="running",
            remote=self.remote,
            root=str(self.run_root),
            endpoint_url=args.endpoint_url,
            source_sha=os.environ.get("GITHUB_SHA", "unknown"),
            workflow_run_id=os.environ.get("GITHUB_RUN_ID", "local"),
            workflow_run_attempt=os.environ.get("GITHUB_RUN_ATTEMPT", "1"),
            crab_version="unknown",
            platform=os.name,
            rustfs_image=os.environ.get("CRAB_RUSTFS_IMAGE", "unknown"),
            env=redact_env(self.env),
        )

    def build_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env.update(
            {
                "AWS_ACCESS_KEY_ID": self.args.access_key,
                "AWS_SECRET_ACCESS_KEY": self.args.secret_key,
                "AWS_REGION": self.args.region,
                "AWS_ENDPOINT_URL": self.args.endpoint_url,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "GIT_TERMINAL_PROMPT": "0",
                "GIT_MERGE_AUTOEDIT": "no",
            }
        )
        return env

    def write_report(self) -> None:
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.report.updated_at = utc_now()
        path = self.artifacts / "report.json"
        path.write_text(
            json.dumps(self.report.__dict__, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        self.report.artifacts["report"] = str(path)

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        self.report.checks.append(
            {
                "name": name,
                "ok": bool(ok),
                "detail": detail or {},
                "checked_at": utc_now(),
            }
        )
        self.write_report()
        if not ok:
            raise SmokeError(f"check failed: {name}: {detail}")

    def run_cmd(
        self,
        name: str,
        command: list[str],
        cwd: Path,
        *,
        check: bool = True,
    ) -> CommandRecord:
        self.command_index += 1
        self.logs.mkdir(parents=True, exist_ok=True)
        stdout_log = self.logs / f"{self.command_index:03}-{slug(name)}.stdout.log"
        stderr_log = self.logs / f"{self.command_index:03}-{slug(name)}.stderr.log"
        start = time.perf_counter()
        with stdout_log.open("wb") as stdout_fh, stderr_log.open("wb") as stderr_fh:
            proc = subprocess.run(
                command,
                cwd=cwd,
                env=self.env,
                stdout=stdout_fh,
                stderr=stderr_fh,
                check=False,
            )
        record = CommandRecord(
            name=name,
            args=command,
            cwd=str(cwd),
            exit_code=proc.returncode,
            duration_ms=int((time.perf_counter() - start) * 1000),
            stdout_log=str(stdout_log),
            stderr_log=str(stderr_log),
        )
        self.report.commands.append(record.__dict__)
        self.write_report()
        if check and proc.returncode != 0:
            stdout_tail = stdout_log.read_text(errors="replace")[-2000:]
            stderr_tail = stderr_log.read_text(errors="replace")[-4000:]
            raise SmokeError(
                f"{name} failed rc={proc.returncode}\n"
                f"STDOUT:\n{stdout_tail}\nSTDERR:\n{stderr_tail}"
            )
        return record

    def crab(self, name: str, args: list[str], cwd: Path, *, check: bool = True) -> CommandRecord:
        return self.run_cmd(name, ["crab", *args], cwd, check=check)

    def git(self, name: str, args: list[str], cwd: Path) -> CommandRecord:
        return self.run_cmd(name, ["git", *args], cwd)

    def text(self, record: CommandRecord) -> str:
        return Path(record.stdout_log).read_text(errors="replace")

    def json_stdout(self, record: CommandRecord) -> Any:
        text = self.text(record).strip()
        if not text:
            raise SmokeError(f"{record.name} produced empty stdout")
        return json.loads(text)

    def jsonl_stdout(self, record: CommandRecord) -> list[dict[str, Any]]:
        rows = []
        for line in self.text(record).splitlines():
            line = line.strip()
            if line:
                rows.append(json.loads(line))
        return rows

    def write(self, path: Path, content: str) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def read_json(self, path: Path) -> Any:
        return json.loads(path.read_text(encoding="utf-8"))

    def enable_workflow_config(self, repo: Path) -> None:
        self.crab("config set workflow enabled", ["config", "set", "workflow.enabled", "true"], repo)
        self.crab("config set workflow parallelism", ["config", "set", "workflow.parallelism", "2"], repo)
        self.crab(
            "config set workflow lock timeout",
            ["config", "set", "workflow.lock_timeout_secs", "30"],
            repo,
        )

    def enable_hydra_config(self, repo: Path) -> None:
        self.crab("config set hydra enabled", ["config", "set", "hydra.enabled", "true"], repo)
        self.crab("config set hydra dir", ["config", "set", "hydra.config_dir", "conf"], repo)
        self.crab("config set hydra name", ["config", "set", "hydra.config_name", "config.yaml"], repo)

    def run_lockfile_resolve_smoke(self) -> None:
        lockfile = self.source / "crab.lock"
        if not lockfile.exists():
            raise SmokeError("workflow lockfile resolve smoke requires source crab.lock")

        resolve_dir = self.run_root / "lockfile-resolve"
        resolve_dir.mkdir(parents=True)
        body = lockfile.read_text(encoding="utf-8")
        conflicted = f"<<<<<<< HEAD\n{body}=======\n{body}>>>>>>> incoming\n"
        (resolve_dir / "crab.lock").write_text(conflicted, encoding="utf-8")

        resolved = self.json_stdout(
            self.crab(
                "workflow lockfile resolve json",
                ["workflow", "lockfile", "resolve", "--json"],
                resolve_dir,
            )
        )
        resolved_body = (resolve_dir / "crab.lock").read_text(encoding="utf-8")
        self.check(
            "workflow-lockfile-resolve-json",
            resolved.get("schema") == "workflow.lockfile_resolve"
            and resolved["data"].get("strategy") == "recompute"
            and "<<<<<<<" not in resolved_body
            and ">>>>>>>" not in resolved_body,
            resolved.get("data", {}),
        )

    def run_split_lockfile_smoke(self) -> None:
        split = self.run_root / "split-lockfile"
        split.mkdir(parents=True)
        self.write(
            split / ".crab/local.toml",
            "[workflow]\nenabled = true\ndiscover = \"recursive\"\n",
        )
        self.write(split / "split-raw.txt", "raw\n")
        self.write(
            split / "train.workflow.yaml",
            """
stages:
  prepare_split:
    cmd: "cp split-raw.txt split-prepared.txt"
    deps:
      - split-raw.txt
    outs:
      - split-prepared.txt
    env: empty
""".lstrip(),
        )
        self.write(
            split / "eval.workflow.yaml",
            """
stages:
  eval_split:
    cmd: "cp split-prepared.txt split-report.txt"
    deps:
      - split-prepared.txt
    outs:
      - split-report.txt
    env: empty
""".lstrip(),
        )

        self.crab("split workflow run recursive", ["run", "--recursive", "--json"], split)
        self.check(
            "workflow-split-seeded-monolithic-lockfile",
            (split / "crab.lock").exists(),
            {"path": str(split / "crab.lock")},
        )
        payload = self.json_stdout(
            self.crab(
                "workflow lockfile split json",
                ["workflow", "lockfile", "split", "--update-config", "--json"],
                split,
            )
        )
        train_lock = split / "train.workflow.lock"
        eval_lock = split / "eval.workflow.lock"
        self.check(
            "workflow-lockfile-split-json",
            payload.get("schema") == "workflow.lockfile_split"
            and payload["data"].get("removed_monolithic") is True
            and train_lock.exists()
            and eval_lock.exists()
            and not (split / "crab.lock").exists(),
            {
                "payload": payload.get("data", {}),
                "train_lock": str(train_lock),
                "eval_lock": str(eval_lock),
            },
        )
        status = self.json_stdout(
            self.crab(
                "workflow status split json",
                ["workflow", "status", "--recursive", "--json"],
                split,
            )
        )
        states = {row["stage"]: row["state"] for row in status["data"]["stages"]}
        self.check(
            "workflow-status-reads-split-lockfiles",
            states == {"train.prepare_split": "up_to_date", "eval.eval_split": "up_to_date"},
            states,
        )

    def run_journal_smoke(self) -> None:
        listing = self.json_stdout(
            self.crab("workflow journal ls json", ["workflow", "journal", "ls", "--json"], self.source)
        )
        journals = listing["data"].get("journals", [])
        self.check("workflow-journal-ls-json", len(journals) > 0, listing.get("data", {}))
        run_id = journals[0]["run_id"]
        shown = self.json_stdout(
            self.crab(
                "workflow journal show json",
                ["workflow", "journal", "show", run_id, "--json"],
                self.source,
            )
        )
        self.check(
            "workflow-journal-show-json",
            shown.get("schema") == "workflow.journal.show"
            and shown["data"].get("run_id") == run_id
            and len(shown["data"].get("stages", [])) > 0,
            shown.get("data", {}),
        )
        dry_gc = self.json_stdout(
            self.crab(
                "workflow journal gc dry json",
                ["workflow", "journal", "gc", "--keep", "1", "--dry-run", "--json"],
                self.source,
            )
        )
        self.check(
            "workflow-journal-gc-dry-json",
            dry_gc.get("schema") == "workflow.journal.gc"
            and dry_gc["data"].get("dry_run") is True
            and "kept" in dry_gc["data"],
            dry_gc.get("data", {}),
        )
        real_gc = self.json_stdout(
            self.crab(
                "workflow journal gc json",
                ["workflow", "journal", "gc", "--keep", "1", "--json"],
                self.source,
            )
        )
        self.check(
            "workflow-journal-gc-json",
            real_gc.get("schema") == "workflow.journal.gc"
            and real_gc["data"].get("dry_run") is False
            and len(real_gc["data"].get("kept", [])) <= 1,
            real_gc.get("data", {}),
        )

    def run_status_target_smoke(self) -> None:
        by_stage = self.json_stdout(
            self.crab("workflow status train target json", ["workflow", "status", "train", "--json"], self.source)
        )
        by_stage_names = [row["stage"] for row in by_stage["data"]["stages"]]
        self.check("workflow-status-target-stage", by_stage_names == ["train"], {"stages": by_stage_names})

        by_out = self.json_stdout(
            self.crab(
                "workflow status output target json",
                ["workflow", "status", "models/model.json", "--json"],
                self.source,
            )
        )
        by_out_names = [row["stage"] for row in by_out["data"]["stages"]]
        self.check("workflow-status-target-output", by_out_names == ["train"], {"stages": by_out_names})

        raw_path = self.source / "data/raw.csv"
        original_raw = raw_path.read_text(encoding="utf-8")
        try:
            raw_path.write_text(original_raw + "4,13\n", encoding="utf-8")
            without_deps = self.json_stdout(
                self.crab(
                    "workflow status train target after upstream change",
                    ["workflow", "status", "train", "--json"],
                    self.source,
                )
            )
            without_deps_states = {row["stage"]: row["state"] for row in without_deps["data"]["stages"]}
            with_deps = self.json_stdout(
                self.crab(
                    "workflow status train with deps after upstream change",
                    ["workflow", "status", "--with-deps", "train", "--json"],
                    self.source,
                )
            )
            with_deps_states = {row["stage"]: row["state"] for row in with_deps["data"]["stages"]}
        finally:
            raw_path.write_text(original_raw, encoding="utf-8")

        self.check(
            "workflow-status-with-deps",
            without_deps_states == {"train": "up_to_date"}
            and with_deps_states.get("prepare") == "stale"
            and with_deps_states.get("train") == "up_to_date",
            {"without_deps": without_deps_states, "with_deps": with_deps_states},
        )

    def run_dag_target_smoke(self) -> None:
        target = self.json_stdout(
            self.crab("workflow dag train target json", ["workflow", "dag", "train", "--json"], self.source)
        )
        target_stages = {row["name"] for row in target["data"]["stages"]}
        full = self.json_stdout(
            self.crab(
                "workflow dag train full json",
                ["workflow", "dag", "train", "--full", "--json"],
                self.source,
            )
        )
        full_stages = {row["name"] for row in full["data"]["stages"]}
        md = self.text(self.crab("workflow dag md", ["workflow", "dag", "--md"], self.source))
        self.check(
            "workflow-dag-target-full-md",
            target_stages == {"prepare", "train"}
            and {"prepare", "train", "export"}.issubset(full_stages)
            and "```mermaid" in md
            and "graph TD" in md,
            {"target": sorted(target_stages), "full": sorted(full_stages), "md": md[:120]},
        )
        self.crab(
            "workflow dag collapse foreach matrix",
            ["workflow", "dag", "--collapse-foreach-matrix"],
            self.source,
        )

    def run_push_cache_smoke(self) -> None:
        before = self.json_stdout(
            self.crab(
                "workflow status cloud before push-cache json",
                ["workflow", "status", "--cloud", "--json"],
                self.source,
            )
        )
        before_remote = before["data"].get("remote", {})
        before_states = {row["stage"]: row.get("remote_state") for row in before["data"]["stages"]}
        self.check(
            "workflow-status-cloud-before-push-cache",
            before_remote.get("new", 0) > 0 and "new" in before_states.values(),
            {"remote": before_remote, "states": before_states},
        )

        pushed = self.json_stdout(
            self.crab(
                "workflow push-cache all json",
                ["workflow", "push-cache", "--all", "--json"],
                self.source,
            )
        )
        data = pushed.get("data", {})
        self.check(
            "workflow-push-cache-all-json",
            pushed.get("schema") == "workflow.push_cache"
            and data.get("errors") == 0
            and data.get("pushed", 0) + data.get("skipped", 0) > 0,
            data,
        )
        remote_prefix = f"{REMOTE_PREFIX}/{self.run_id}/refs/crab/stages/"
        listed: dict[str, Any] = {}
        listed_count = 0
        for attempt in range(1, 6):
            listed = self.json_stdout(
                self.run_cmd(
                    f"list workflow cache refs attempt {attempt}",
                    [
                        "aws",
                        "s3api",
                        "list-objects-v2",
                        "--bucket",
                        self.args.bucket,
                        "--prefix",
                        remote_prefix,
                        "--endpoint-url",
                        self.args.endpoint_url,
                        "--output",
                        "json",
                    ],
                    self.run_root,
                )
            )
            listed_count = int(listed.get("KeyCount") or len(listed.get("Contents", [])))
            if listed_count > 0:
                break
            time.sleep(1)
        self.check(
            "workflow-push-cache-remote-refs",
            listed_count > 0,
            {"prefix": remote_prefix, "key_count": listed_count},
        )
        after = self.json_stdout(
            self.crab(
                "workflow status cloud after push-cache json",
                ["workflow", "status", "--cloud", "--json"],
                self.source,
            )
        )
        after_remote = after["data"].get("remote", {})
        after_states = {row["stage"]: row.get("remote_state") for row in after["data"]["stages"]}
        self.check(
            "workflow-status-cloud-after-push-cache",
            after_remote.get("checked", 0) > 0
            and after_remote.get("new") == 0
            and after_remote.get("deleted") == 0
            and after_remote.get("missing") == 0
            and after_remote.get("checked")
            == after_remote.get("in_sync", 0) + after_remote.get("uncached", 0)
            and set(after_states.values()).issubset({"in_sync", "uncached"})
            and "in_sync" in after_states.values(),
            {"remote": after_remote, "states": after_states},
        )

    def init_git_repo(self, repo: Path, label: str) -> None:
        self.git(f"git init {label}", ["init", "-b", "main"], repo)
        self.git(f"git config email {label}", ["config", "user.email", "dvc-e2e@crab.local"], repo)
        self.git(f"git config name {label}", ["config", "user.name", "Crab DVC E2E"], repo)
        self.git(f"git config gpgsign {label}", ["config", "commit.gpgsign", "false"], repo)

    def preflight(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        usage = shutil.disk_usage(self.root)
        self.check(
            "workspace-free-space",
            usage.free >= self.args.min_free_bytes,
            {
                "free_gib": round(usage.free / 1024**3, 2),
                "required_bytes": self.args.min_free_bytes,
            },
        )
        try:
            with urllib.request.urlopen(self.args.endpoint_url, timeout=5) as response:
                status = response.status
        except urllib.error.HTTPError as exc:
            status = exc.code
        self.check("rustfs-endpoint-reachable", status < 500, {"status": status})
        self.run_cmd(
            "rustfs head bucket",
            [
                "aws",
                "s3api",
                "head-bucket",
                "--bucket",
                self.args.bucket,
                "--endpoint-url",
                self.args.endpoint_url,
            ],
            self.run_root,
        )

    def create_fixture(self, external_dep: str, external_out: Path) -> None:
        self.write(
            self.source / ".gitignore",
            ".crab/\n.crab/workflow/\ncache-only-out/\n",
        )
        self.write(
            self.source / "params.yaml",
            params_yaml(2.0, 1.0),
        )
        self.write(self.source / "data/raw.csv", "id,value\n1,2\n2,5\n3,8\n")
        self.write(
            self.source / "scripts/prepare.py",
            """
import csv
from pathlib import Path

def load_params():
    params = {}
    section = None
    for line in Path('params.yaml').read_text().splitlines():
        if not line.strip():
            continue
        if not line.startswith(' '):
            section = line.strip().rstrip(':')
            params[section] = {}
            continue
        key, value = line.strip().split(':', 1)
        params[section][key.strip()] = float(value.strip())
    return params

params = load_params()
scale = float(params['model']['scale'])
Path('data').mkdir(exist_ok=True)
with open('data/raw.csv', newline='') as src, open('data/prepared.csv', 'w', newline='') as dst:
    reader = csv.DictReader(src)
    writer = csv.DictWriter(dst, fieldnames=['id', 'feature'])
    writer.writeheader()
    for row in reader:
        writer.writerow({'id': row['id'], 'feature': float(row['value']) * scale})
""".lstrip(),
        )
        self.write(
            self.source / "scripts/train.py",
            """
import csv, json
from pathlib import Path

def load_params():
    params = {}
    section = None
    for line in Path('params.yaml').read_text().splitlines():
        if not line.strip():
            continue
        if not line.startswith(' '):
            section = line.strip().rstrip(':')
            params[section] = {}
            continue
        key, value = line.strip().split(':', 1)
        params[section][key.strip()] = float(value.strip())
    return params

params = load_params()
bias = float(params['model']['bias'])
features = []
with open('data/prepared.csv', newline='') as src:
    for row in csv.DictReader(src):
        features.append(float(row['feature']) + bias)
score = sum(features) / len(features)
Path('models').mkdir(exist_ok=True)
Path('plots').mkdir(exist_ok=True)
Path('metrics.json').write_text(json.dumps({'score': score, 'count': len(features)}, sort_keys=True) + '\\n')
Path('models/model.json').write_text(json.dumps({'bias': bias, 'score': score}, sort_keys=True) + '\\n')
with open('plots/loss.csv', 'w', newline='') as dst:
    writer = csv.DictWriter(dst, fieldnames=['step', 'loss'])
    writer.writeheader()
    for i, value in enumerate(features, 1):
        writer.writerow({'step': i, 'loss': round(score / (i + value), 6)})
""".lstrip(),
        )
        self.write(
            self.source / "scripts/export.py",
            f"""
from pathlib import Path
out = Path({str(external_out)!r})
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text('external-report-v1\\n')
Path('exports').mkdir(exist_ok=True)
Path('exports/done.txt').write_text('done\\n')
""".lstrip(),
        )
        self.write(
            self.source / "scripts/external_check.py",
            "from pathlib import Path\nPath('external_dep_seen.txt').write_text('ok\\n')\n",
        )
        self.report.artifacts["external_dep"] = external_dep
        self.report.artifacts["external_out"] = str(external_out)

    def run_hydra_smoke(self) -> None:
        self.hydra.mkdir(parents=True)
        self.init_git_repo(self.hydra, "hydra")
        self.crab("hydra crab init", ["init", self.hydra_remote], self.hydra)
        self.enable_workflow_config(self.hydra)
        self.enable_hydra_config(self.hydra)
        self.write(self.hydra / ".gitignore", ".crab/\n")
        self.write(
            self.hydra / "conf/config.yaml",
            "defaults:\n  - train/model: resnet\n  - train/optimizer: sgd\n",
        )
        self.write(
            self.hydra / "conf/train/model/resnet.yaml",
            "name: ResNet\nsize: 50\n",
        )
        self.write(
            self.hydra / "conf/train/model/efficientnet.yaml",
            "name: EfficientNet\nsize: b0\n",
        )
        self.write(
            self.hydra / "conf/train/optimizer/sgd.yaml",
            "name: SGD\nlr: 0.001\n",
        )
        self.write(
            self.hydra / "crab.yaml",
            """
params:
  - params.yaml
stages:
  hydra_train:
    cmd: "python3 scripts/hydra_train.py"
    deps:
      - params.yaml
      - scripts/hydra_train.py
    outs:
      - hydra_out.txt
    metrics:
      - hydra_metrics.json
    params:
      - train.model.name
      - train.model.size
      - train.optimizer.lr
""".lstrip(),
        )
        self.write(
            self.hydra / "scripts/hydra_train.py",
            """
import json
from pathlib import Path

def parse_params():
    data = {}
    stack = [(0, data)]
    for raw in Path('params.yaml').read_text().splitlines():
        if not raw.strip():
            continue
        indent = len(raw) - len(raw.lstrip(' '))
        key, _, value = raw.strip().partition(':')
        while stack and indent <= stack[-1][0] and len(stack) > 1:
            stack.pop()
        parent = stack[-1][1]
        if value.strip():
            text = value.strip()
            try:
                parsed = float(text)
            except ValueError:
                parsed = text
            parent[key] = parsed
        else:
            child = {}
            parent[key] = child
            stack.append((indent, child))
    return data

params = parse_params()
train = params['train']
Path('hydra_out.txt').write_text(
    f"{train['model']['name']}|{train['model']['size']}|{train['optimizer']['lr']}\\n"
)
Path('hydra_metrics.json').write_text(json.dumps({
    'model': train['model']['name'],
    'lr': train['optimizer']['lr'],
}, sort_keys=True) + '\\n')
""".lstrip(),
        )
        self.git("hydra add baseline", ["add", ".gitignore", "conf", "crab.yaml", "scripts"], self.hydra)
        self.git("hydra commit baseline", ["commit", "-m", "hydra dvc e2e"], self.hydra)
        self.crab("hydra push baseline", ["push", "--jsonl"], self.hydra)
        hydra_exp = self.json_stdout(
            self.crab(
                "hydra exp run json",
                [
                    "exp",
                    "run",
                    "-S",
                    "train/model=efficientnet",
                    "-S",
                    "train.optimizer.lr=0.02",
                    "-n",
                    "hydra-efficient",
                    "--json",
                ],
                self.hydra,
            )
        )
        hydra_exp_id = self.find_experiment_id(hydra_exp)
        self.report.artifacts["hydra_exp_id"] = hydra_exp_id
        self.check("hydra-exp-run-json", "data" in hydra_exp, hydra_exp.get("data", {}))
        hydra_show = self.json_stdout(self.crab("hydra exp show json", ["exp", "show", hydra_exp_id, "--json"], self.hydra))
        self.check("hydra-exp-show-json", "data" in hydra_show, hydra_show.get("data", {}))
        self.crab("hydra exp apply json", ["exp", "apply", hydra_exp_id, "--json"], self.hydra)
        hydra_out = (self.hydra / "hydra_out.txt").read_text(encoding="utf-8")
        self.check(
            "hydra-exp-apply-output",
            hydra_out == "EfficientNet|b0|0.02\n",
            {"hydra_out": hydra_out},
        )
        hydra_metrics = self.json_stdout(self.crab("hydra metrics show json", ["metrics", "show", "--json"], self.hydra))
        self.check("hydra-metrics-show-json", "data" in hydra_metrics, hydra_metrics.get("data", {}))
        self.crab("hydra exp push all json", ["exp", "push", "--all", "--json"], self.hydra)

    def find_experiment_id(self, payload: Any) -> str:
        data = payload.get("data", payload) if isinstance(payload, dict) else payload
        stack: list[Any] = [data]
        while stack:
            item = stack.pop()
            if isinstance(item, dict):
                for key, value in item.items():
                    if key in {"id", "experiment_id", "exp_id"} and isinstance(value, str):
                        return value
                    if isinstance(value, (dict, list)):
                        stack.append(value)
            elif isinstance(item, list):
                stack.extend(item)
        raise SmokeError(f"could not find experiment id in payload: {payload}")

    def find_task_id(self, *payloads: Any) -> str | None:
        stack: list[Any] = list(payloads)
        while stack:
            item = stack.pop()
            if isinstance(item, dict):
                for key, value in item.items():
                    if key in {"id", "task_id"} and isinstance(value, str):
                        return value
                    if isinstance(value, (dict, list)):
                        stack.append(value)
            elif isinstance(item, list):
                stack.extend(item)
        return None

    def run(self) -> int:
        if self.run_root.exists():
            raise SmokeError(f"run root already exists: {self.run_root}")
        self.source.mkdir(parents=True)
        self.logs.mkdir(parents=True)
        self.write_report()
        try:
            self.preflight()
            version = self.run_cmd("crab version", ["crab", "--version"], self.run_root)
            self.report.crab_version = self.text(version).strip()
            self.write_report()
            self.git("git init source", ["init", "-b", "main"], self.source)
            self.git("git config email", ["config", "user.email", "dvc-e2e@crab.local"], self.source)
            self.git("git config name", ["config", "user.name", "Crab DVC E2E"], self.source)
            self.git("git config gpgsign", ["config", "commit.gpgsign", "false"], self.source)
            self.crab("crab init", ["init", self.remote], self.source)
            self.enable_workflow_config(self.source)

            external_payload = self.artifacts / "external-input.txt"
            external_payload.write_text("external-seed-v1\n", encoding="utf-8")
            external_key = f"{REMOTE_PREFIX}/{self.run_id}/external/input.txt"
            self.run_cmd(
                "put external s3 dep",
                [
                    "aws",
                    "s3api",
                    "put-object",
                    "--bucket",
                    self.args.bucket,
                    "--key",
                    external_key,
                    "--body",
                    str(external_payload),
                    "--endpoint-url",
                    self.args.endpoint_url,
                ],
                self.run_root,
            )
            external_dep = f"s3://{self.args.bucket}/{external_key}"
            external_out = self.run_root / "external" / "report.txt"
            self.create_fixture(external_dep, external_out)

            self.crab(
                "stage add prepare",
                [
                    "stage",
                    "add",
                    "-n",
                    "prepare",
                    "-d",
                    "data/raw.csv",
                    "-p",
                    "model.scale",
                    "-o",
                    "data/prepared.csv",
                    "--desc",
                    "prepare features",
                    "python3",
                    "scripts/prepare.py",
                ],
                self.source,
            )
            self.crab(
                "stage add train",
                [
                    "stage",
                    "add",
                    "-n",
                    "train",
                    "-d",
                    "data/prepared.csv",
                    "-p",
                    "model.bias",
                    "-m",
                    "metrics.json",
                    "--plots",
                    "plots/loss.csv",
                    "-o",
                    "models/model.json",
                    "--desc",
                    "train model",
                    "python3",
                    "scripts/train.py",
                ],
                self.source,
            )
            self.crab(
                "stage add export external out",
                [
                    "stage",
                    "add",
                    "-n",
                    "export",
                    "-d",
                    "models/model.json",
                    "-O",
                    str(external_out),
                    "-o",
                    "exports/done.txt",
                    "python3",
                    "scripts/export.py",
                ],
                self.source,
            )
            self.crab(
                "stage add external dep",
                [
                    "stage",
                    "add",
                    "-n",
                    "external_check",
                    "-d",
                    external_dep,
                    "-o",
                    "external_dep_seen.txt",
                    "python3",
                    "scripts/external_check.py",
                ],
                self.source,
            )

            stage_list = self.json_stdout(self.crab("stage list json", ["stage", "list", "--json"], self.source))
            self.check(
                "stage-list-json",
                "data" in stage_list and len(stage_list["data"].get("stages", [])) >= 4,
                stage_list.get("data", {}),
            )
            dag = self.json_stdout(self.crab("workflow dag json", ["workflow", "dag", "--json"], self.source))
            self.check(
                "workflow-dag-json",
                "data" in dag and len(dag["data"].get("stages", [])) >= 4,
                dag.get("data", {}),
            )
            self.crab("workflow dag mermaid", ["workflow", "dag", "--mermaid"], self.source)
            self.crab("workflow dag dot outs", ["workflow", "dag", "--dot", "--outs"], self.source)
            self.crab("workflow validate dry", ["run", "--validate", "--json"], self.source)
            self.crab("workflow dry run", ["run", "--dry", "--json"], self.source)

            run_record = self.crab(
                "workflow run cache push jsonl",
                ["run", "--cache-push", "--jsonl"],
                self.source,
            )
            events = self.jsonl_stdout(run_record)
            schemas = {event.get("schema") for event in events}
            self.check(
                "workflow-run-jsonl-events",
                any(schema and str(schema).startswith("workflow.stage") for schema in schemas),
                {"schemas": sorted(str(schema) for schema in schemas)},
            )
            self.check(
                "metrics-produced",
                self.read_json(self.source / "metrics.json")["score"] == 11.0,
                self.read_json(self.source / "metrics.json"),
            )
            self.check(
                "external-output-written",
                external_out.read_text(encoding="utf-8") == "external-report-v1\n",
                {"path": str(external_out)},
            )
            self.run_lockfile_resolve_smoke()
            self.run_journal_smoke()
            self.run_split_lockfile_smoke()
            self.run_status_target_smoke()
            self.run_dag_target_smoke()

            status = self.json_stdout(self.crab("workflow status json", ["workflow", "status", "--json"], self.source))
            states = {row["stage"]: row["state"] for row in status["data"]["stages"]}
            self.check(
                "workflow-status-up-to-date",
                all(state == "up_to_date" for state in states.values()),
                states,
            )
            why = self.json_stdout(
                self.crab(
                    "workflow why train json",
                    ["workflow", "status", "--why", "train", "--json"],
                    self.source,
                )
            )
            self.check(
                "workflow-why-train",
                why["data"]["stage"] == "train" and why["data"]["up_to_date"],
                why["data"],
            )
            self.crab("repro single item train", ["repro", "--single-item", "train", "--json"], self.source)

            self.git("git add baseline", ["add", "."], self.source)
            self.git("git commit baseline", ["commit", "-m", f"dvc e2e baseline {self.run_id}"], self.source)
            self.crab("crab push baseline", ["push", "--jsonl"], self.source)

            metrics_show = self.json_stdout(self.crab("metrics show json", ["metrics", "show", "--json"], self.source))
            self.check("metrics-show-json", "data" in metrics_show, metrics_show.get("data", {}))
            params_show = self.json_stdout(self.crab("params show json", ["params", "show", "--json"], self.source))
            self.check("params-show-json", "data" in params_show, params_show.get("data", {}))
            plots_show = self.json_stdout(
                self.crab("plots show vega json", ["plots", "show", "--show-vega", "--json"], self.source)
            )
            self.check("plots-show-json", "data" in plots_show, plots_show.get("data", {}))
            self.crab("plots templates", ["plots", "templates"], self.source)

            self.write(
                self.source / "params.yaml",
                params_yaml(3.0, 1.5),
            )
            self.crab("workflow rerun changed params", ["run", "--json"], self.source)
            self.check(
                "metrics-changed-after-param-update",
                self.read_json(self.source / "metrics.json")["score"] == 16.5,
                self.read_json(self.source / "metrics.json"),
            )
            self.run_push_cache_smoke()
            self.crab("params diff json", ["params", "diff", "--json"], self.source)
            self.crab("metrics diff json", ["metrics", "diff", "--json"], self.source)
            self.crab("plots diff vega json", ["plots", "diff", "--show-vega", "--json"], self.source)
            self.git("git add changed", ["add", "."], self.source)
            self.git("git commit changed", ["commit", "-m", "dvc e2e changed params"], self.source)
            self.crab("crab push changed", ["push", "--jsonl"], self.source)

            freeze = self.json_stdout(self.crab("freeze train json", ["freeze", "train", "--json"], self.source))
            self.check("freeze-json", "data" in freeze, freeze.get("data", {}))
            frozen_status = self.json_stdout(
                self.crab("workflow status frozen json", ["workflow", "status", "--json"], self.source)
            )
            frozen_states = {row["stage"]: row["state"] for row in frozen_status["data"]["stages"]}
            self.check("train-frozen-state", frozen_states.get("train") == "frozen", frozen_states)
            unfreeze = self.json_stdout(self.crab("unfreeze train json", ["unfreeze", "train", "--json"], self.source))
            self.check("unfreeze-json", "data" in unfreeze, unfreeze.get("data", {}))

            exp1 = self.json_stdout(
                self.crab(
                    "exp run scale4 json",
                    ["exp", "run", "-S", "model.scale=4.0", "-n", "scale4", "-m", "scale four", "--json"],
                    self.source,
                )
            )
            exp2 = self.json_stdout(
                self.crab(
                    "exp run scale5 json",
                    ["exp", "run", "-S", "model.scale=5.0", "-n", "scale5", "-m", "scale five", "--json"],
                    self.source,
                )
            )
            exp1_id = self.find_experiment_id(exp1)
            exp2_id = self.find_experiment_id(exp2)
            self.report.artifacts["exp1_id"] = exp1_id
            self.report.artifacts["exp2_id"] = exp2_id
            exp_ls = self.json_stdout(self.crab("exp ls json", ["exp", "ls", "--json"], self.source))
            self.check("exp-ls-json", "data" in exp_ls, exp_ls.get("data", {}))
            exp_show = self.json_stdout(self.crab("exp show json", ["exp", "show", "--json"], self.source))
            self.check("exp-show-json", "data" in exp_show, exp_show.get("data", {}))
            self.crab("exp show one json", ["exp", "show", exp1_id, "--json"], self.source)
            self.crab("exp diff json", ["exp", "diff", exp1_id, exp2_id, "--json"], self.source)
            self.crab("exp rename", ["exp", "rename", exp1_id, "scale4-renamed"], self.source)
            self.crab(
                "exp promote scale5 json",
                ["exp", "promote", exp2_id, f"exp-scale5-{self.run_id}", "--json"],
                self.source,
            )
            apply_payload = self.json_stdout(
                self.crab("exp apply scale5 json", ["exp", "apply", exp2_id, "--json"], self.source)
            )
            self.check("exp-apply-json", "data" in apply_payload, apply_payload.get("data", {}))
            push_payload = self.json_stdout(
                self.crab("exp push all json", ["exp", "push", "--all", "--json"], self.source)
            )
            self.check("exp-push-json", "data" in push_payload, push_payload.get("data", {}))
            remove_payload = self.json_stdout(
                self.crab("exp remove scale4 json", ["exp", "remove", exp1_id, "--json"], self.source)
            )
            self.check("exp-remove-json", "data" in remove_payload, remove_payload.get("data", {}))
            pull_payload = self.json_stdout(
                self.crab("exp pull scale4 json", ["exp", "pull", exp1_id, "--json"], self.source)
            )
            self.check("exp-pull-json", "data" in pull_payload, pull_payload.get("data", {}))
            gc_payload = self.json_stdout(
                self.crab("exp gc dry json", ["exp", "gc", "--keep", "20", "--dry-run", "--json"], self.source)
            )
            self.check("exp-gc-json", "data" in gc_payload, gc_payload.get("data", {}))
            self.crab(
                "exp save json",
                ["exp", "save", "-n", "workspace-save", "-m", "save current workspace", "--json"],
                self.source,
            )

            queue_payload = self.json_stdout(
                self.crab(
                    "exp queue json",
                    ["exp", "queue", "-S", "model.scale=6.0", "-m", "queued scale six", "--json"],
                    self.source,
                )
            )
            self.check("exp-queue-json", "data" in queue_payload, queue_payload.get("data", {}))
            queue_before = self.json_stdout(
                self.crab("queue status before json", ["queue", "status", "--json"], self.source)
            )
            self.check("queue-status-before-json", "data" in queue_before, queue_before.get("data", {}))
            self.crab("queue start json", ["queue", "start", "--jobs", "1", "--json"], self.source)
            queue_after = self.json_stdout(
                self.crab("queue status after json", ["queue", "status", "--json"], self.source)
            )
            self.check("queue-status-after-json", "data" in queue_after, queue_after.get("data", {}))
            task_id = self.find_task_id(queue_payload.get("data"), queue_before.get("data"), queue_after.get("data"))
            if task_id:
                self.crab("queue logs json", ["queue", "logs", task_id, "--json"], self.source, check=False)
            self.crab("queue remove success json", ["queue", "remove", "--success", "--json"], self.source, check=False)
            self.crab("exp clean json", ["exp", "clean", "--json"], self.source, check=False)

            self.run_cmd("crab clone remote", ["crab", "clone", self.remote, str(self.clone)], self.run_root)
            self.enable_workflow_config(self.clone)
            clone_status = self.json_stdout(
                self.crab("clone workflow status json", ["workflow", "status", "--json"], self.clone)
            )
            self.check("clone-workflow-status-json", "data" in clone_status, clone_status.get("data", {}))
            self.crab("clone run cache-only", ["run", "--cache-only", "--json"], self.clone, check=False)

            self.run_hydra_smoke()

            self.report.status = "ok"
            self.write_report()
            print(
                json.dumps(
                    {
                        "status": "ok",
                        "run_id": self.run_id,
                        "root": str(self.run_root),
                        "remote": self.remote,
                        "report": str(self.artifacts / "report.json"),
                    },
                    indent=2,
                )
            )
            return 0
        except Exception as exc:
            self.report.status = "failed"
            self.report.artifacts["failure"] = str(exc)
            self.write_report()
            print(
                json.dumps(
                    {
                        "status": "failed",
                        "run_id": self.run_id,
                        "root": str(self.run_root),
                        "remote": self.remote,
                        "report": str(self.artifacts / "report.json"),
                        "error": str(exc),
                    },
                    indent=2,
                ),
                flush=True,
            )
            return 1


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--bucket", default=DEFAULT_BUCKET)
    parser.add_argument("--endpoint-url", default=DEFAULT_ENDPOINT)
    parser.add_argument("--region", default="us-east-1")
    parser.add_argument("--access-key", default="crab")
    parser.add_argument("--secret-key", default="crab")
    parser.add_argument("--run-id")
    parser.add_argument(
        "--min-free-bytes",
        type=int,
        default=20 * 1024**3,
        help="Minimum free space required before running the smoke (default: 20 GiB).",
    )
    return parser.parse_args()


def main() -> int:
    return Smoke(parse_args()).run()


if __name__ == "__main__":
    raise SystemExit(main())
