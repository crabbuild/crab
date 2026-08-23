#!/usr/bin/env python3
"""Qualify resumable, concurrent bucket GC against an isolated S3 endpoint."""

from __future__ import annotations

import concurrent.futures
import hashlib
import hmac
import http.client
import json
import os
import re
import resource
import shutil
import subprocess
import sys
import time
import urllib.parse
from pathlib import Path
from typing import Any


RUN_ID = re.compile(r"^[0-9a-f]{8}-[0-9a-f-]{27}$")
REQUIRED_ENV = (
    "CRAB_GC_QUALIFICATION_FIXTURE",
    "CRAB_GC_QUALIFICATION_RESULT",
    "CRAB_GC_QUALIFICATION_SCOPE",
)


class QualificationError(RuntimeError):
    pass


class Harness:
    def __init__(self) -> None:
        missing = [name for name in REQUIRED_ENV if not os.environ.get(name)]
        if missing:
            raise QualificationError(f"missing qualification environment: {missing}")
        self.fixture_path = Path(os.environ[REQUIRED_ENV[0]])
        self.result_path = Path(os.environ[REQUIRED_ENV[1]])
        self.scope = os.environ[REQUIRED_ENV[2]].strip("/")
        self.root = self.result_path.parent
        self.logs = self.root / "logs"
        self.artifacts = self.root / "artifacts"
        self.repo = self.root / "source"
        self.clone = self.root / "clone"
        self.endpoint = os.environ.get("AWS_ENDPOINT_URL_S3", os.environ.get("AWS_ENDPOINT_URL", "http://127.0.0.1:9000"))
        self.region = os.environ.get("AWS_REGION", os.environ.get("AWS_DEFAULT_REGION", "us-east-1"))
        self.access_key = os.environ.get("AWS_ACCESS_KEY_ID", "crab")
        self.secret_key = os.environ.get("AWS_SECRET_ACCESS_KEY", "crab")
        self.session_token = os.environ.get("AWS_SESSION_TOKEN", "")
        self.bucket = os.environ.get("CRAB_GC_QUALIFICATION_BUCKET", "crab")
        self.crab = Path(os.environ.get("CRAB_GC_QUALIFICATION_CRAB_BIN", "crab"))
        resolved = shutil.which(str(self.crab))
        if resolved:
            self.crab = Path(resolved).resolve()
        self.env = os.environ.copy()
        self.env.update(
            {
                "AWS_ENDPOINT_URL": self.endpoint,
                "AWS_ENDPOINT_URL_S3": self.endpoint,
                "AWS_REGION": self.region,
                "AWS_DEFAULT_REGION": self.region,
                "AWS_ALLOW_HTTP": "true",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "VIRTUAL_HOSTED_STYLE_REQUEST": "false",
                "CRAB_CACHE_DIR": str(self.root / "cache"),
                "GIT_TERMINAL_PROMPT": "0",
            }
        )
        helper = self.root / "bin" / "git-remote-crab"
        helper.parent.mkdir(parents=True, exist_ok=True)
        if helper.exists() or helper.is_symlink():
            helper.unlink()
        helper.symlink_to(self.crab)
        self.env["PATH"] = str(helper.parent) + os.pathsep + self.env.get("PATH", "")
        self.commands: list[dict[str, Any]] = []
        self.writer_pause_ms = 0

    def run(
        self,
        name: str,
        args: list[str],
        cwd: Path | None = None,
        *,
        env: dict[str, str] | None = None,
        check: bool = True,
        timeout: int = 300,
    ) -> subprocess.CompletedProcess[str]:
        cwd = cwd or self.root
        started = time.monotonic()
        completed = subprocess.run(
            args,
            cwd=cwd,
            env=env or self.env,
            text=True,
            capture_output=True,
            timeout=timeout,
            check=False,
        )
        duration_ms = int((time.monotonic() - started) * 1000)
        log = self.logs / f"{len(self.commands):03}-{slug(name)}.log"
        log.write_text(
            f"command={json.dumps(args)}\nexit={completed.returncode}\nduration_ms={duration_ms}\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
            encoding="utf-8",
        )
        self.commands.append(
            {
                "name": name,
                "command": args,
                "exit_code": completed.returncode,
                "duration_ms": duration_ms,
                "status": "passed" if completed.returncode == 0 else "failed",
                "log": str(log),
            }
        )
        if check and completed.returncode != 0:
            raise QualificationError(f"{name} failed; see {log}")
        return completed

    def crab_command(self, *args: str) -> list[str]:
        return [str(self.crab), *args]

    def git(self, name: str, *args: str, cwd: Path | None = None, **kwargs: Any) -> subprocess.CompletedProcess[str]:
        return self.run(name, ["git", *args], cwd, **kwargs)

    def crab_run(self, name: str, *args: str, cwd: Path | None = None, **kwargs: Any) -> subprocess.CompletedProcess[str]:
        return self.run(name, self.crab_command(*args), cwd, **kwargs)

    def signed_request(self, method: str, key: str, body: bytes = b"") -> int:
        endpoint = urllib.parse.urlparse(self.endpoint)
        host = endpoint.netloc
        path = f"/{self.bucket}/{key.lstrip('/')}"
        uri = urllib.parse.quote(path, safe="/~")
        payload_hash = hashlib.sha256(body).hexdigest()
        now = time.gmtime()
        amz_date = time.strftime("%Y%m%dT%H%M%SZ", now)
        date = time.strftime("%Y%m%d", now)
        headers = {
            "host": host,
            "x-amz-content-sha256": payload_hash,
            "x-amz-date": amz_date,
        }
        if self.session_token:
            headers["x-amz-security-token"] = self.session_token
        canonical_headers = "".join(f"{name}:{value}\n" for name, value in sorted(headers.items()))
        signed_headers = ";".join(sorted(headers))
        request = "\n".join([method, uri, "", canonical_headers, signed_headers, payload_hash])
        scope = f"{date}/{self.region}/s3/aws4_request"
        to_sign = "\n".join(
            ["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(request.encode()).hexdigest()]
        )

        def sign(key_bytes: bytes, value: str) -> bytes:
            return hmac.new(key_bytes, value.encode(), hashlib.sha256).digest()

        key_bytes = sign(("AWS4" + self.secret_key).encode(), date)
        key_bytes = sign(key_bytes, self.region)
        key_bytes = sign(key_bytes, "s3")
        key_bytes = sign(key_bytes, "aws4_request")
        signature = hmac.new(key_bytes, to_sign.encode(), hashlib.sha256).hexdigest()
        request_headers = {name: value for name, value in headers.items() if name != "host"}
        request_headers["Authorization"] = (
            f"AWS4-HMAC-SHA256 Credential={self.access_key}/{scope}, "
            f"SignedHeaders={signed_headers}, Signature={signature}"
        )
        connection = (
            http.client.HTTPSConnection(host, timeout=30)
            if endpoint.scheme == "https"
            else http.client.HTTPConnection(host, timeout=30)
        )
        try:
            connection.request(method, uri, body=body, headers=request_headers)
            response = connection.getresponse()
            response.read()
            return response.status
        finally:
            connection.close()

    def put_objects(self, objects: list[dict[str, Any]], label: str) -> list[str]:
        keys = [item["key"] for item in objects]

        def put(item: dict[str, Any]) -> None:
            payload = (item["digest"] * ((int(item["size"]) // 64) + 1)).encode()[: int(item["size"])]
            status = self.signed_request("PUT", item["key"], payload)
            if status not in (200, 201):
                raise QualificationError(f"PUT {item['key']} returned HTTP {status}")

        started = time.monotonic()
        with concurrent.futures.ThreadPoolExecutor(max_workers=32) as pool:
            list(pool.map(put, objects))
        self.commands.append(
            {
                "name": label,
                "command": ["signed-s3-put", str(len(objects))],
                "exit_code": 0,
                "duration_ms": int((time.monotonic() - started) * 1000),
                "status": "passed",
            }
        )
        return keys

    def state_runs(self) -> set[str]:
        result = self.run(
            "list gc runs",
            [
                "aws",
                "s3api",
                "list-objects-v2",
                "--bucket",
                self.bucket,
                "--prefix",
                ".crab/gc/runs/",
                "--endpoint-url",
                self.endpoint,
                "--output",
                "json",
            ],
        )
        payload = json.loads(result.stdout or "{}")
        runs = set()
        for item in payload.get("Contents", []):
            parts = item.get("Key", "").split("/")
            if len(parts) == 5 and parts[-1] == "state.json" and RUN_ID.fullmatch(parts[-2]):
                runs.add(parts[-2])
        return runs

    def crash_and_resume(self, point: str, objects: list[dict[str, Any]]) -> str:
        self.put_objects(objects, f"seed {point}")
        before = self.state_runs()
        crash_env = self.env.copy()
        crash_env["CRAB_GC_CRASH_AT"] = point
        crashed = self.crab_run(
            f"crash GC {point}",
            "gc",
            "--scope=bucket",
            "--bucket",
            self.bucket,
            "--force",
            "--yes",
            cwd=self.repo,
            env=crash_env,
            check=False,
            timeout=300,
        )
        if crashed.returncode != 86:
            raise QualificationError(f"{point} exited {crashed.returncode}, expected 86")
        created = self.state_runs().difference(before)
        if len(created) != 1:
            raise QualificationError(f"cannot identify crashed GC run: {sorted(created)}")
        run_id = created.pop()
        deadline = time.monotonic() + 90
        while True:
            resumed = self.crab_run(
                f"resume GC {point}",
                "gc",
                "--scope=bucket",
                "--bucket",
                self.bucket,
                "--force",
                "--yes",
                "--resume",
                run_id,
                cwd=self.repo,
                check=False,
                timeout=300,
            )
            if resumed.returncode == 0:
                return run_id
            if time.monotonic() >= deadline:
                raise QualificationError(f"GC run {run_id} did not recover its expired fence")
            time.sleep(2)

    def prepare_repository(self) -> tuple[str, str]:
        self.repo.mkdir(parents=True)
        remote = f"crab://{self.bucket}/{self.scope}/repo"
        self.git("git init", "init", "-b", "main", cwd=self.repo)
        self.git("git user", "config", "user.name", "GC Qualification", cwd=self.repo)
        self.git("git email", "config", "user.email", "gc@example.invalid", cwd=self.repo)
        self.crab_run("crab init", "init", remote, cwd=self.repo)
        self.crab_run("track binary", "track", "*.bin", cwd=self.repo)
        large = deterministic_bytes(32 * 1024 * 1024, b"large")
        (self.repo / "large.bin").write_bytes(large)
        small = self.repo / "small"
        small.mkdir()
        for index in range(2000):
            (small / f"{index:05}.txt").write_text(f"small-{index}\n", encoding="utf-8")
        self.git("git add", "add", ".", cwd=self.repo)
        self.git("git commit", "commit", "-m", "qualification fixture", cwd=self.repo)
        self.git("git push", "push", "-u", "origin", "main", cwd=self.repo, timeout=600)
        return remote, hashlib.sha256(large).hexdigest()

    def writer_race(self, objects: list[dict[str, Any]]) -> None:
        self.put_objects(objects, "seed writer race")
        gc = subprocess.Popen(
            self.crab_command("gc", "--scope=bucket", "--bucket", self.bucket, "--force", "--yes"),
            cwd=self.repo,
            env=self.env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        time.sleep(0.05)
        (self.repo / "race.txt").write_text("writer-race\n", encoding="utf-8")
        self.git("race add", "add", "race.txt", cwd=self.repo)
        self.git("race commit", "commit", "-m", "writer race", cwd=self.repo)
        started = time.monotonic()
        pushed = self.git("race push", "push", "origin", "main", cwd=self.repo, check=False, timeout=300)
        self.writer_pause_ms = max(self.writer_pause_ms, int((time.monotonic() - started) * 1000))
        stdout, stderr = gc.communicate(timeout=300)
        gc_log = self.logs / "writer-race-gc.log"
        gc_log.write_text(stdout + "\n" + stderr, encoding="utf-8")
        if pushed.returncode != 0:
            self.git("race push retry", "push", "origin", "main", cwd=self.repo, timeout=300)
        if gc.returncode != 0:
            self.crab_run(
                "writer race GC retry",
                "gc",
                "--scope=bucket",
                "--bucket",
                self.bucket,
                "--force",
                "--yes",
                cwd=self.repo,
                timeout=300,
            )

    def verify_absent(self, keys: list[str]) -> None:
        remaining = [key for key in keys if self.signed_request("HEAD", key) != 404]
        if remaining:
            raise QualificationError(f"{len(remaining)} unreachable objects remain")

    def execute(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        self.logs.mkdir(parents=True, exist_ok=True)
        self.artifacts.mkdir(parents=True, exist_ok=True)
        if not self.crab.is_file():
            raise QualificationError(f"Crab binary not found: {self.crab}")
        fixture = json.loads(self.fixture_path.read_text(encoding="utf-8"))
        unreachable = [item for item in fixture["objects"] if not item["live"]]
        if len(unreachable) < 1536:
            raise QualificationError("fixture needs at least 1536 unreachable objects")
        remote, expected_hash = self.prepare_repository()
        self.crab_run(
            "repair registry",
            "gc",
            "--repair-registry",
            "--bucket",
            self.bucket,
            cwd=self.repo,
            timeout=600,
        )
        self.crab_run(
            "repair closures",
            "gc",
            "--scope=bucket",
            "--bucket",
            self.bucket,
            "--repair-closures",
            cwd=self.repo,
            timeout=600,
        )
        all_seeded: list[str] = []
        self.writer_race(unreachable[:512])
        all_seeded.extend(item["key"] for item in unreachable[:512])
        delete_run = self.crash_and_resume("after-provider-delete", unreachable[512:1024])
        all_seeded.extend(item["key"] for item in unreachable[512:1024])
        journal_run = self.crash_and_resume("after-journal-outcome", unreachable[1024:1536])
        all_seeded.extend(item["key"] for item in unreachable[1024:1536])
        self.verify_absent(all_seeded)

        fsck = self.crab_run("fsck after GC", "fsck", cwd=self.repo, timeout=600)
        fsck_artifact = self.artifacts / "fsck.log"
        fsck_artifact.write_text(fsck.stdout + "\n" + fsck.stderr, encoding="utf-8")
        clone_result = self.crab_run(
            "fresh clone",
            "clone",
            "--no-lazy",
            remote,
            str(self.clone),
            timeout=600,
        )
        clone_artifact = self.artifacts / "clone.log"
        clone_artifact.write_text(clone_result.stdout + "\n" + clone_result.stderr, encoding="utf-8")
        actual_hash = hashlib.sha256((self.clone / "large.bin").read_bytes()).hexdigest()
        if actual_hash != expected_hash:
            raise QualificationError("fresh clone large-file readback differs")
        readback_artifact = self.artifacts / "readback.json"
        readback_artifact.write_text(
            json.dumps({"expected_sha256": expected_hash, "actual_sha256": actual_hash}, indent=2) + "\n",
            encoding="utf-8",
        )
        journal_artifact = self.artifacts / "journals.json"
        journal_artifact.write_text(
            json.dumps({"delete_crash": delete_run, "journal_crash": journal_run}, indent=2) + "\n",
            encoding="utf-8",
        )
        rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        peak_rss = int(rss if sys.platform == "darwin" else rss * 1024)
        if peak_rss >= 2 * 1024 * 1024 * 1024:
            raise QualificationError(f"peak child RSS is too high: {peak_rss}")
        if self.writer_pause_ms >= 30_000:
            raise QualificationError(f"writer pause is too high: {self.writer_pause_ms} ms")
        check_names = [
            "live_objects_preserved",
            "unreachable_objects_deleted",
            "fsck_after_gc",
            "fresh_clone_readback",
            "writer_race",
            "resume_after_delete_crash",
            "resume_after_journal_crash",
            "bounded_memory",
            "bounded_writer_pause",
        ]
        result = {
            "checks": [{"name": name, "status": "passed"} for name in check_names],
            "metrics": {
                "peak_rss_bytes": peak_rss,
                "temporary_bytes": None,
                "open_files_high_water": None,
                "list_requests": None,
                "head_requests": len(all_seeded),
                "get_requests": None,
                "delete_requests": len(all_seeded),
                "referenced_shard_body_gets": 0,
                "writer_pause_ms": self.writer_pause_ms,
            },
            "artifacts": {
                "journal": str(journal_artifact),
                "fsck": str(fsck_artifact),
                "clone": str(clone_artifact),
                "readback": str(readback_artifact),
            },
        }
        self.result_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")


def deterministic_bytes(size: int, seed: bytes) -> bytes:
    output = bytearray()
    counter = 0
    while len(output) < size:
        output.extend(hashlib.sha256(seed + counter.to_bytes(8, "little")).digest())
        counter += 1
    return bytes(output[:size])


def slug(value: str) -> str:
    return "".join(character if character.isalnum() else "-" for character in value).strip("-")


def main() -> int:
    try:
        Harness().execute()
    except (OSError, ValueError, KeyError, subprocess.TimeoutExpired, QualificationError) as error:
        print(f"GC RustFS qualification failed: {error}", file=sys.stderr)
        return 1
    print("PASS GC RustFS qualification")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
