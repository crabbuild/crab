#!/usr/bin/env python3
"""Qualify multipart orphan cleanup against the packaged gateway and RustFS."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


GATEWAY_CONTAINER = "crab-s3-gateway"
GATEWAY_ENDPOINT = "http://127.0.0.1:18080"
MANAGEMENT_ENDPOINT = "http://127.0.0.1:18081"
BACKEND_ENDPOINT = "http://127.0.0.1:19000"
GATEWAY_BUCKET = "gateway-repository"
BACKEND_BUCKET = "crab-s3-gateway-qualification"
MULTIPART_PREFIX = "repositories/qualification/s3/multipart"
COMMAND_TIMEOUT_SECONDS = 120


class QualificationError(RuntimeError):
    """The multipart cleanup qualification did not meet its contract."""


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise QualificationError(message)


def _required_env(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise QualificationError(f"required environment variable {name} is missing")
    return value


def _credential_env(access_key: str, secret_key: str, config: Path) -> dict[str, str]:
    environment = os.environ.copy()
    environment.update(
        {
            "AWS_ACCESS_KEY_ID": access_key,
            "AWS_SECRET_ACCESS_KEY": secret_key,
            "AWS_DEFAULT_REGION": "us-east-1",
            "AWS_REGION": "us-east-1",
            "AWS_CONFIG_FILE": str(config),
            "AWS_EC2_METADATA_DISABLED": "true",
        }
    )
    environment.pop("AWS_SESSION_TOKEN", None)
    return environment


def _run(
    *command: str,
    environment: dict[str, str] | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        check=check,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=COMMAND_TIMEOUT_SECONDS,
    )


class AwsClient:
    def __init__(self, endpoint: str, environment: dict[str, str]) -> None:
        self.endpoint = endpoint
        self.environment = environment

    def run(
        self, *arguments: str, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        return _run(
            "aws",
            "--endpoint-url",
            self.endpoint,
            *arguments,
            environment=self.environment,
            check=check,
        )

    def json(self, *arguments: str) -> dict[str, Any]:
        result = self.run(*arguments, "--output", "json")
        value = json.loads(result.stdout)
        if not isinstance(value, dict):
            raise QualificationError("AWS CLI returned a non-object response")
        return value

    def get_json(self, bucket: str, key: str, output: Path) -> dict[str, Any]:
        self.run(
            "s3api",
            "get-object",
            "--bucket",
            bucket,
            "--key",
            key,
            str(output),
        )
        value = json.loads(output.read_text(encoding="utf-8"))
        if not isinstance(value, dict):
            raise QualificationError("stored multipart record is not an object")
        return value

    def put_json(self, bucket: str, key: str, value: dict[str, Any], source: Path) -> None:
        temporary = source.with_suffix(source.suffix + ".tmp")
        temporary.write_text(json.dumps(value, sort_keys=True) + "\n", encoding="utf-8")
        temporary.replace(source)
        self.run(
            "s3api",
            "put-object",
            "--bucket",
            bucket,
            "--key",
            key,
            "--body",
            str(source),
        )


def _create_upload(client: AwsClient, key: str) -> str:
    response = client.json(
        "s3api",
        "create-multipart-upload",
        "--bucket",
        GATEWAY_BUCKET,
        "--key",
        key,
    )
    upload_id = response.get("UploadId")
    if not isinstance(upload_id, str) or not upload_id:
        raise QualificationError("gateway did not return a multipart upload ID")
    return upload_id


def _upload_part(client: AwsClient, key: str, upload_id: str, part_file: Path) -> None:
    client.run(
        "s3api",
        "upload-part",
        "--bucket",
        GATEWAY_BUCKET,
        "--key",
        key,
        "--upload-id",
        upload_id,
        "--part-number",
        "1",
        "--body",
        str(part_file),
    )


def _part_count(client: AwsClient, upload_id: str) -> int:
    response = client.json(
        "s3api",
        "list-objects-v2",
        "--bucket",
        BACKEND_BUCKET,
        "--prefix",
        f"{MULTIPART_PREFIX}/parts/{upload_id}/",
    )
    contents = response.get("Contents") or []
    if not isinstance(contents, list):
        raise QualificationError("backend multipart listing is malformed")
    return len(contents)


def _metric_value(metrics: str, exact_name: str) -> int:
    for line in metrics.splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[0] == exact_name:
            return int(float(fields[1]))
    return 0


def _completed_scans(metrics: str) -> int:
    return sum(
        _metric_value(
            metrics,
            f'crab_s3_gateway_multipart_maintenance_cycles_total{{outcome="{outcome}"}}',
        )
        for outcome in ("success", "degraded", "clock_error")
    )


def _wait_for_readiness() -> None:
    for _ in range(60):
        result = _run(
            "docker",
            "exec",
            GATEWAY_CONTAINER,
            "crab-s3-gateway",
            "--config",
            "/etc/crab/s3-gateway.toml",
            "--readiness-check",
            check=False,
        )
        if result.returncode == 0:
            return
        time.sleep(1)
    raise QualificationError("restarted gateway did not become ready")


def _wait_for_cleanup() -> tuple[int, int]:
    for _ in range(60):
        try:
            with urllib.request.urlopen(f"{MANAGEMENT_ENDPOINT}/metrics", timeout=5) as response:
                metrics = response.read().decode("utf-8")
        except (OSError, UnicodeDecodeError, urllib.error.URLError):
            time.sleep(1)
            continue
        scans = _completed_scans(metrics)
        cleanups = _metric_value(
            metrics,
            'crab_s3_gateway_multipart_maintenance_actions_total{action="missing_session_cleanup"}',
        )
        if scans >= 1 and cleanups >= 1:
            return scans, cleanups
        time.sleep(1)
    raise QualificationError("orphan was not reclaimed by a completed maintenance scan")


def _scan_logs_for_credentials(root: Path) -> None:
    access_key = _required_env("GATEWAY_ACCESS_KEY")
    session_access_key = _required_env("GATEWAY_SESSION_ACCESS_KEY")
    candidates = [
        _required_env("GATEWAY_SECRET_KEY"),
        _required_env("GATEWAY_SESSION_SECRET_KEY"),
        _required_env("GATEWAY_SESSION_TOKEN"),
        _required_env("RUSTFS_ACCESS_KEY"),
        _required_env("RUSTFS_SECRET_KEY"),
        f"Credential={access_key}",
        f"Credential={session_access_key}",
        f"AWS {access_key}:",
        f"AWSAccessKeyId={access_key}",
        "X-Amz-Signature=",
        "Authorization: AWS",
        "Authorization: AWS4-",
    ]
    log_path = root / "cleanup-gateway.log"
    with log_path.open("w", encoding="utf-8") as output:
        subprocess.run(
            ("docker", "logs", GATEWAY_CONTAINER),
            check=True,
            stdout=output,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=COMMAND_TIMEOUT_SECONDS,
        )
    with log_path.open(encoding="utf-8", errors="replace") as lines:
        exposed = any(
            candidate in line
            for line in lines
            for candidate in candidates
        )
    _require(not exposed, "gateway logs exposed credentials")


def qualify(root: Path, part_file: Path, proof_path: Path) -> None:
    root.mkdir(parents=True, exist_ok=True)
    _require(
        part_file.is_file() and part_file.stat().st_size == 8 * 1024 * 1024,
        "part fixture must be exactly 8 MiB",
    )
    config = root / "aws-config"
    gateway = AwsClient(
        GATEWAY_ENDPOINT,
        _credential_env(
            _required_env("GATEWAY_ACCESS_KEY"),
            _required_env("GATEWAY_SECRET_KEY"),
            config,
        ),
    )
    backend = AwsClient(
        BACKEND_ENDPOINT,
        _credential_env(
            _required_env("RUSTFS_ACCESS_KEY"),
            _required_env("RUSTFS_SECRET_KEY"),
            config,
        ),
    )

    keys = {
        "live": "main/qualification/cleanup-live.bin",
        "frozen": "main/qualification/cleanup-frozen.bin",
        "orphan": "main/qualification/cleanup-orphan.bin",
    }
    upload_ids = {name: _create_upload(gateway, key) for name, key in keys.items()}
    for name, key in keys.items():
        _upload_part(gateway, key, upload_ids[name], part_file)
    parts_before = {
        name: _part_count(backend, upload_id)
        for name, upload_id in upload_ids.items()
    }
    _require(
        parts_before == {"live": 1, "frozen": 1, "orphan": 1},
        "multipart fixtures were not staged",
    )

    _run("docker", "kill", "--signal", "KILL", GATEWAY_CONTAINER)
    exit_code = int(
        _run(
            "docker",
            "inspect",
            GATEWAY_CONTAINER,
            "--format",
            "{{.State.ExitCode}}",
        ).stdout.strip()
    )
    _require(exit_code == 137, "gateway did not terminate from SIGKILL")

    frozen_state_key = f"{MULTIPART_PREFIX}/uploads/{upload_ids['frozen']}/state.json"
    frozen_state_path = root / "cleanup-frozen-state.json"
    frozen = backend.get_json(BACKEND_BUCKET, frozen_state_key, frozen_state_path)
    parts = frozen.get("parts")
    _require(
        isinstance(parts, dict) and isinstance(parts.get("1"), dict),
        "frozen part record is missing",
    )
    frozen_etag = parts["1"].get("etag")
    _require(isinstance(frozen_etag, str) and bool(frozen_etag), "frozen part ETag is missing")
    frozen["state"] = "completing"
    frozen["selected_parts"] = [[1, frozen_etag]]
    frozen["revision"] = int(frozen["revision"]) + 1
    backend.put_json(BACKEND_BUCKET, frozen_state_key, frozen, frozen_state_path)

    orphan_state_key = f"{MULTIPART_PREFIX}/uploads/{upload_ids['orphan']}/state.json"
    orphan_state_path = root / "cleanup-orphan-state.json"
    orphan = backend.get_json(BACKEND_BUCKET, orphan_state_key, orphan_state_path)
    capacity_slot = orphan.get("capacity_slot")
    _require(type(capacity_slot) is int and capacity_slot >= 0, "orphan capacity slot is invalid")
    capacity_key = f"{MULTIPART_PREFIX}/capacity/{capacity_slot:05d}.json"
    capacity_path = root / "cleanup-orphan-capacity.json"
    capacity = backend.get_json(BACKEND_BUCKET, capacity_key, capacity_path)
    owner = capacity.get("owner")
    _require(
        isinstance(owner, dict) and owner.get("upload_id") == upload_ids["orphan"],
        "orphan capacity owner is invalid",
    )
    expired = int(time.time()) - 1
    owner["created_seconds"] = expired - 1
    owner["expires_seconds"] = expired
    backend.put_json(BACKEND_BUCKET, capacity_key, capacity, capacity_path)
    backend.run("s3api", "delete-object", "--bucket", BACKEND_BUCKET, "--key", orphan_state_key)

    _run("docker", "start", GATEWAY_CONTAINER)
    _wait_for_readiness()
    completed_scans, missing_cleanups = _wait_for_cleanup()
    _require(1 <= completed_scans <= 2, "orphan cleanup exceeded two completed scans")
    _require(missing_cleanups == 1, "maintenance did not report exactly one orphan cleanup")

    parts_after = {
        name: _part_count(backend, upload_id)
        for name, upload_id in upload_ids.items()
    }
    live_state = backend.get_json(
        BACKEND_BUCKET,
        f"{MULTIPART_PREFIX}/uploads/{upload_ids['live']}/state.json",
        root / "cleanup-live-state-after.json",
    )
    frozen = backend.get_json(
        BACKEND_BUCKET,
        frozen_state_key,
        root / "cleanup-frozen-state-after.json",
    )
    orphan_exists = backend.run(
        "s3api",
        "head-object",
        "--bucket",
        BACKEND_BUCKET,
        "--key",
        orphan_state_key,
        check=False,
    ).returncode == 0
    capacity = backend.get_json(
        BACKEND_BUCKET,
        capacity_key,
        root / "cleanup-orphan-capacity-after.json",
    )
    live_open = live_state.get("state") == "open"
    frozen_completing = frozen.get("state") == "completing"
    capacity_released = capacity.get("owner") is None
    _require(
        parts_after == {"live": 1, "frozen": 1, "orphan": 0},
        "cleanup deleted a protected part or retained the orphan",
    )
    _require(live_open and frozen_completing, "cleanup changed protected lifecycle state")
    _require(not orphan_exists and capacity_released, "cleanup retained orphan state or capacity")

    frozen["state"] = "open"
    frozen["selected_parts"] = None
    frozen["revision"] = int(frozen["revision"]) + 1
    backend.put_json(
        BACKEND_BUCKET,
        frozen_state_key,
        frozen,
        root / "cleanup-frozen-state-open.json",
    )
    for name in ("live", "frozen"):
        gateway.run(
            "s3api",
            "abort-multipart-upload",
            "--bucket",
            GATEWAY_BUCKET,
            "--key",
            keys[name],
            "--upload-id",
            upload_ids[name],
        )
    fixtures_reclaimed = all(_part_count(backend, upload_ids[name]) == 0 for name in upload_ids)
    _require(fixtures_reclaimed, "qualification retained staged multipart payloads")
    _scan_logs_for_credentials(root)

    proof = {
        "completed_cleanup_scans": completed_scans,
        "missing_session_cleanups": missing_cleanups,
        "live_parts_before": parts_before["live"],
        "live_parts_after": parts_after["live"],
        "frozen_parts_before": parts_before["frozen"],
        "frozen_parts_after": parts_after["frozen"],
        "orphan_parts_before": parts_before["orphan"],
        "orphan_parts_after": parts_after["orphan"],
        "live_session_open": live_open,
        "frozen_session_completing": frozen_completing,
        "orphan_session_absent": not orphan_exists,
        "orphan_capacity_released": capacity_released,
        "fixture_payloads_reclaimed": fixtures_reclaimed,
        "forced_exit_code": exit_code,
    }
    proof_path.parent.mkdir(parents=True, exist_ok=True)
    temporary = proof_path.with_suffix(proof_path.suffix + ".tmp")
    temporary.write_text(json.dumps(proof, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(proof_path)


def parser() -> argparse.ArgumentParser:
    argument_parser = argparse.ArgumentParser()
    argument_parser.add_argument("--qualification-root", type=Path, required=True)
    argument_parser.add_argument("--part-file", type=Path, required=True)
    argument_parser.add_argument("--proof", type=Path, required=True)
    return argument_parser


def main() -> int:
    args = parser().parse_args()
    try:
        qualify(args.qualification_root, args.part_file, args.proof)
    except (
        QualificationError,
        OSError,
        ValueError,
        json.JSONDecodeError,
        subprocess.SubprocessError,
    ) as error:
        print(f"error: multipart cleanup qualification failed: {error}", file=sys.stderr)
        return 1
    print(args.proof)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
