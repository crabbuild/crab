#!/usr/bin/env python3
"""Qualify SDK transfers against an owned native RustFS on macOS or Windows."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[2]
RELEASE = "1.0.3"
# Official release digests pin the executable used by each platform cell.
ASSETS = {
    ("Darwin", "arm64"): (
        "rustfs-macos-aarch64-v1.0.3.zip",
        "9ec6f2ad89df88db9b8889f59e37bb27f6b67c61ee8a97968dd90aaa2ac4dd30",
    ),
    ("Windows", "AMD64"): (
        "rustfs-windows-x86_64-v1.0.3.zip",
        "c72e573b8dbadd1b294d76b46a33ff296afde01a52c3c321da579e74edf5ed32",
    ),
}


def install_rustfs(directory: Path) -> tuple[Path, str, str]:
    asset, digest = ASSETS[(platform.system(), platform.machine())]
    directory.mkdir(parents=True, exist_ok=True)
    archive = directory / asset
    if not archive.exists():
        url = f"https://github.com/rustfs/rustfs/releases/download/{RELEASE}/{asset}"
        with urllib.request.urlopen(url, timeout=120) as source, archive.open("wb") as target:
            shutil.copyfileobj(source, target)
    with archive.open("rb") as source:
        if hashlib.file_digest(source, "sha256").hexdigest() != digest:
            raise RuntimeError("RustFS release checksum mismatch")
    name = "rustfs.exe" if os.name == "nt" else "rustfs"
    executable = directory / name
    with zipfile.ZipFile(archive) as source:
        # Extract one known member; archive paths never select destination paths.
        candidates = [entry for entry in source.infolist() if Path(entry.filename).name == name]
        if len(candidates) != 1:
            raise RuntimeError("RustFS archive must contain exactly one server executable")
        with source.open(candidates[0]) as binary, executable.open("wb") as target:
            shutil.copyfileobj(binary, target)
    executable.chmod(0o700)
    return executable, asset, digest


def build_tests(output: Path, toolchain: str) -> dict[str, Path]:
    artifacts = output / "compiler-artifacts.jsonl"
    with artifacts.open("w") as stdout, (output / "build.log").open("w") as stderr:
        subprocess.run(
            ["cargo", f"+{toolchain}", "test", "-p", "crab-sdk", "--locked", "--all-features",
             "--lib", "--test", "remote_blob", "--test", "remote_write", "--test",
             "local_round_trip", "--no-run", "--message-format=json"],
            cwd=ROOT, stdout=stdout, stderr=stderr, check=True,
        )
        subprocess.run(
            ["cargo", f"+{toolchain}", "build", "-p", "crab-sdk", "--locked",
             "--all-features", "--examples"],
            cwd=ROOT, stdout=stderr, stderr=stderr, check=True,
        )
    wanted = {"crab_sdk", "remote_blob", "remote_write", "local_round_trip"}
    executables: dict[str, Path] = {}
    for line in artifacts.read_text().splitlines():
        artifact = json.loads(line)
        name = artifact.get("target", {}).get("name")
        if (artifact.get("reason") == "compiler-artifact" and name in wanted
                and artifact.get("profile", {}).get("test") and artifact.get("executable")):
            if name in executables:
                raise RuntimeError(f"duplicate SDK test executable: {name}")
            executables[name] = Path(artifact["executable"])
    if executables.keys() != wanted or not all(path.is_file() for path in executables.values()):
        raise RuntimeError("SDK test artifact set is incomplete")
    return executables


def fixture_environment(work: Path, output: Path, endpoint: str, bucket: str,
                        git: Path, crab: Path, target: Path) -> dict[str, str]:
    # Isolate provider selection from developer profiles and previous CI fixtures.
    environment = {name: value for name, value in os.environ.items()
                   if not name.startswith(("AWS_", "RUSTFS_", "CRAB_SDK_TEST_"))}
    config = work / "aws-config"
    config.write_text("[default]\nregion = us-east-1\ns3 =\n    addressing_style = path\n")
    credentials = work / "aws-credentials"
    credentials.touch()
    global_git = work / "gitconfig"
    global_git.touch()
    cache = work / "cache"
    cache.mkdir()
    environment.update({
        "AWS_ACCESS_KEY_ID": secrets.token_hex(10),
        "AWS_SECRET_ACCESS_KEY": secrets.token_hex(24),
        "AWS_REGION": "us-east-1", "AWS_DEFAULT_REGION": "us-east-1",
        "AWS_EC2_METADATA_DISABLED": "true", "AWS_MAX_ATTEMPTS": "1",
        "AWS_ENDPOINT_URL": endpoint, "AWS_ENDPOINT_URL_S3": endpoint,
        "AWS_ALLOW_HTTP": "true", "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
        "AWS_CONFIG_FILE": str(config), "AWS_SHARED_CREDENTIALS_FILE": str(credentials),
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": str(global_git),
        "GIT_TERMINAL_PROMPT": "0", "CRAB_CACHE_DIR": str(cache),
        "TMPDIR": str(work), "TMP": str(work), "TEMP": str(work),
        "CRAB_SDK_TEST_S3_BUCKET": bucket, "CRAB_SDK_TEST_S3_ENDPOINT": endpoint,
        "CRAB_SDK_TEST_GIT_BIN": str(git), "CRAB_SDK_TEST_CRAB_BIN": str(crab),
        "CRAB_SDK_TEST_EXAMPLES": str(target / "debug/examples"),
        "CRAB_SDK_TEST_PYTHON": sys.executable,
        "CRAB_SDK_TEST_READ_GUARD": str(ROOT / "crab/scripts/verify-sdk-read-only.py"),
        "CRAB_SDK_TEST_READ_GUARD_REPORT": str(output / "read-transport.json"),
        "QUALIFICATION_BUCKET": bucket, "QUALIFICATION_PREFIX": "qualification/sdk-ref-native",
    })
    return environment


def aws(environment: dict[str, str], *arguments: str) -> dict:
    result = subprocess.run(
        [sys.executable, "-m", "awscli", "--endpoint-url", environment["AWS_ENDPOINT_URL"],
         *arguments, "--output", "json"],
        env=environment, capture_output=True, text=True, timeout=30,
    )
    if result.returncode:
        # Diagnostics must not print provider inputs or signed requests.
        raise RuntimeError(f"fixture AWS CLI operation failed: {' '.join(arguments[:2])}")
    return json.loads(result.stdout or "{}")


def cleanup_bucket(environment: dict[str, str], bucket: str) -> None:
    # This bucket exists only in the fresh server owned by this invocation.
    aws(environment, "s3", "rm", f"s3://{bucket}", "--recursive", "--quiet")
    for upload in aws(environment, "s3api", "list-multipart-uploads", "--bucket", bucket).get("Uploads", []):
        aws(environment, "s3api", "abort-multipart-upload", "--bucket", bucket,
            "--key", upload["Key"], "--upload-id", upload["UploadId"])
    # AWS CLI aggregates Contents across pages and omits per-page KeyCount.
    if aws(environment, "s3api", "list-objects-v2", "--bucket", bucket).get("Contents"):
        raise RuntimeError("qualification objects remain after cleanup")
    if aws(environment, "s3api", "list-multipart-uploads", "--bucket", bucket).get("Uploads"):
        raise RuntimeError("qualification multipart uploads remain after cleanup")
    aws(environment, "s3api", "delete-bucket", "--bucket", bucket)


def qualify(output: Path, tests: dict[str, Path], environment: dict[str, str],
            toolchain: str, git: Path, crab: Path) -> None:
    sources = [ROOT / f"crates/crab-sdk/tests/{name}.rs"
               for name in ("remote_blob", "remote_write", "local_round_trip")]
    digest = hashlib.sha256(b"".join(path.read_bytes() for path in sources)).hexdigest()
    cases = [
        ("remote_blob", "s3_pack_reads_survive_source_removal_and_keep_snapshots_pinned"),
        ("remote_write", "remote_edit_round_trip_without_git"),
        ("remote_write", "stale_ref_batch_changes_no_refs"),
        ("remote_write", "initialization_preserves_foreign_content"),
        ("crab_sdk", "client::mutation::tests::ref_batches_rustfs_recover_without_git_after_restart"),
        ("local_round_trip", "stage_commit_push_round_trip"),
        ("local_round_trip", "fetch_checkout_and_pull_round_trip"),
        ("local_round_trip", "conflict_continue_and_abort_round_trip"),
    ]
    failed = []
    for target, case in cases:
        print(f"Qualifying {case}", flush=True)
        arguments = [
            sys.executable, str(ROOT / "crab/scripts/qualify_sdk.py"), "run",
            "--output", str(output / f"{case.rsplit('::', 1)[-1]}.json"),
            "--backend", "native-rustfs", "--features", "remote,content,write,local,managed",
            "--fixture-digest", digest, "--expected-tests", "1", "--toolchain", toolchain,
            "--git", str(git), "--crab", str(crab), "--timeout-seconds", "1800",
        ]
        if target == "remote_blob":
            arguments += ["--transport-report", environment["CRAB_SDK_TEST_READ_GUARD_REPORT"]]
        if target == "local_round_trip":
            arguments += ["--require-rss-below", "2147483648"]
        result = subprocess.run(
            [*arguments, "--", str(tests[target]), case, "--exact", "--ignored", "--nocapture"],
            cwd=ROOT, env=environment, check=False,
        )
        if result.returncode:
            failed.append(case)
    if failed:
        raise RuntimeError(f"native RustFS qualification failed: {', '.join(failed)}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--toolchain", required=True)
    parser.add_argument("--crab", type=Path, required=True)
    args = parser.parse_args()
    if (platform.system(), platform.machine()) not in ASSETS:
        parser.error("native RustFS qualification supports macOS arm64 and Windows x86_64")
    target = Path(os.environ["CARGO_TARGET_DIR"]).resolve(strict=True)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    selected_git = shutil.which("git")
    if selected_git is None:
        parser.error("Git must be installed on PATH")
    git, crab = Path(selected_git).resolve(strict=True), args.crab.resolve(strict=True)
    tests = build_tests(output, args.toolchain)
    executable, asset, digest = install_rustfs(output / "tools")
    report = {"release": RELEASE, "asset": asset, "sha256": digest,
              "terminal_state": "failed", "cleanup": "not_started"}
    with tempfile.TemporaryDirectory(prefix="rfs-", dir=output) as temporary:
        work = Path(temporary)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        endpoint = f"http://127.0.0.1:{port}"
        bucket = f"sdk-{secrets.token_hex(8)}"
        environment = fixture_environment(work, output, endpoint, bucket, git, crab, target)
        server_environment = {**environment,
                              "RUSTFS_ACCESS_KEY": environment["AWS_ACCESS_KEY_ID"],
                              "RUSTFS_SECRET_KEY": environment["AWS_SECRET_ACCESS_KEY"],
                              "RUSTFS_CONSOLE_ENABLE": "false"}
        data = work / "data"
        data.mkdir()
        created = False
        qualified = False
        with (work / "server.log").open("w") as log:
            server = subprocess.Popen(
                [str(executable), "server", "--address", f"127.0.0.1:{port}", str(data)],
                cwd=work, env=server_environment, stdout=log, stderr=log,
            )
            try:
                deadline = time.monotonic() + 120
                while True:
                    if server.poll() is not None:
                        raise RuntimeError("native RustFS exited before readiness")
                    try:
                        aws(environment, "s3api", "list-buckets")
                        break
                    except RuntimeError:
                        if time.monotonic() >= deadline:
                            raise RuntimeError("native RustFS readiness timed out") from None
                        time.sleep(1)
                aws(environment, "s3api", "create-bucket", "--bucket", bucket)
                created = True
                qualify(output, tests, environment, args.toolchain, git, crab)
                qualified = True
            finally:
                try:
                    report["cleanup"] = "failed"
                    if created:
                        cleanup_bucket(environment, bucket)
                    report["cleanup"] = "passed"
                    if qualified:
                        report["terminal_state"] = "passed"
                finally:
                    server.terminate()
                    try:
                        server.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait()
                    log.flush()
                    contents = (work / "server.log").read_text(errors="replace")
                    for name in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"):
                        contents = contents.replace(environment[name], "[redacted]")
                    (output / "rustfs.log").write_text(contents)
                    (output / "service.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
