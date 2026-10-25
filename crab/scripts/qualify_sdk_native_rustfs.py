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
        "CRAB_SDK_TEST_S3_CLEANUP": "fixture",
        "CRAB_SDK_TEST_GIT_BIN": str(git), "CRAB_SDK_TEST_CRAB_BIN": str(crab),
        "CRAB_SDK_TEST_EXAMPLES": str(target / "debug/examples"),
        "CRAB_SDK_TEST_PYTHON": sys.executable,
        "CRAB_SDK_TEST_READ_GUARD": str(ROOT / "crab/scripts/verify-sdk-read-only.py"),
        "CRAB_SDK_TEST_READ_GUARD_REPORT": str(output / "read-transport.json"),
        "QUALIFICATION_BUCKET": bucket, "QUALIFICATION_PREFIX": "qualification/sdk-ref-native",
    })
    if os.name == "nt":
        # Keep fixture cleanup on the core single-key DELETE API. The separate
        # preflight still verifies DeleteObjects directly against RustFS.
        environment["AWS_DISABLE_BULK_DELETE"] = "true"
    return environment


def rustfs_work_location(output: Path) -> tuple[Path, str, str]:
    # Deep RustFS recovery keys fail during Windows cleanup unless the fixture
    # leaves enough path budget for the object's metadata files.
    if os.name == "nt":
        return output.parent.parent, "r", "d"
    return output.parent, "rfs-", "data"


def aws(environment: dict[str, str], *arguments: str) -> dict:
    result = subprocess.run(
        [sys.executable, "-m", "awscli", "--endpoint-url", environment["AWS_ENDPOINT_URL"],
         *arguments, "--output", "json"],
        env=environment, capture_output=True, text=True, timeout=30,
    )
    if result.returncode:
        diagnostic = result.stderr.strip()
        for name in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"):
            diagnostic = diagnostic.replace(environment[name], "[redacted]")
        raise RuntimeError(f"fixture AWS CLI operation failed: {' '.join(arguments[:2])}: {diagnostic}")
    return json.loads(result.stdout or "{}")


def preflight_backend(environment: dict[str, str], work: Path, bucket: str) -> None:
    body = work / "probe-body"
    received = work / "probe-received"
    content = b"SDK storage contract probe\n"
    body.write_bytes(content)
    # The SDK dependency uses bulk deletion even for a single key by default.
    # Keep a sibling while deleting nested keys to exercise parent pruning too.
    retained = "qualification/backend-probe/retained"
    aws(environment, "s3api", "put-object", "--bucket", bucket, "--key", retained,
        "--body", str(body))
    for multipart, bulk in ((False, False), (False, True), (True, False), (True, True)):
        key = f"qualification/backend-probe/{multipart}/{bulk}"
        if multipart:
            upload = aws(environment, "s3api", "create-multipart-upload", "--bucket", bucket,
                         "--key", key)["UploadId"]
            part = aws(environment, "s3api", "upload-part", "--bucket", bucket, "--key", key,
                       "--upload-id", upload, "--part-number", "1", "--body", str(body))
            parts = json.dumps({"Parts": [{"ETag": part["ETag"], "PartNumber": 1}]})
            aws(environment, "s3api", "complete-multipart-upload", "--bucket", bucket,
                "--key", key, "--upload-id", upload, "--multipart-upload", parts)
        else:
            aws(environment, "s3api", "put-object", "--bucket", bucket, "--key", key,
                "--body", str(body))
        aws(environment, "s3api", "get-object", "--bucket", bucket, "--key", key, str(received))
        if received.read_bytes() != content:
            raise RuntimeError("fixture backend returned incorrect object bytes")
        if bulk:
            request = json.dumps({"Objects": [{"Key": key}], "Quiet": False})
            result = aws(environment, "s3api", "delete-objects", "--bucket", bucket,
                         "--delete", request)
            deleted = [entry.get("Key") for entry in result.get("Deleted", [])]
            if result.get("Errors") or deleted != [key]:
                raise RuntimeError("fixture backend did not confirm bulk deletion")
        else:
            aws(environment, "s3api", "delete-object", "--bucket", bucket, "--key", key)
        aws(environment, "s3api", "get-object", "--bucket", bucket,
            "--key", retained, str(received))
        if received.read_bytes() != content:
            raise RuntimeError("fixture backend changed a retained sibling")
    aws(environment, "s3api", "delete-object", "--bucket", bucket, "--key", retained)
    if aws(environment, "s3api", "list-objects-v2", "--bucket", bucket).get("Contents"):
        raise RuntimeError("fixture backend retained deleted probe objects")


def qualify(output: Path, tests: dict[str, Path], environment: dict[str, str],
            toolchain: str, git: Path, crab: Path) -> None:
    sources = [ROOT / f"crates/crab-sdk/tests/{name}.rs"
               for name in ("remote_blob", "remote_write", "local_round_trip")]
    sources.append(ROOT / "crates/crab-sdk/tests/remote_blob/large_tree.rs")
    digest = hashlib.sha256(b"".join(path.read_bytes() for path in sources)).hexdigest()
    cases = [
        ("remote_blob", "s3_pack_reads_survive_source_removal_and_keep_snapshots_pinned"),
        ("remote_blob", "large_tree::s3_ten_thousand_files_paginate_and_archive_without_source"),
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
        if case == "s3_pack_reads_survive_source_removal_and_keep_snapshots_pinned":
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
    report = {"release": RELEASE, "asset": asset, "sha256": digest, "preflight": "not_started",
              "terminal_state": "failed", "cleanup": "not_started"}
    parent, prefix, data_name = rustfs_work_location(output)
    with tempfile.TemporaryDirectory(prefix=prefix, dir=parent) as temporary:
        work = Path(temporary)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        endpoint = f"http://127.0.0.1:{port}"
        # Keep RustFS's Windows bucket/key/metadata paths below MAX_PATH.
        bucket = "sdk"
        environment = fixture_environment(work, output, endpoint, bucket, git, crab, target)
        server_environment = {**environment,
                              "RUSTFS_ACCESS_KEY": environment["AWS_ACCESS_KEY_ID"],
                              "RUSTFS_SECRET_KEY": environment["AWS_SECRET_ACCESS_KEY"],
                              "RUST_LOG": "warn,rustfs_ecstore=debug",
                              "RUSTFS_CONSOLE_ENABLE": "false"}
        data = work / data_name
        data.mkdir()
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
                report["preflight"] = "failed"
                preflight_backend(environment, work, bucket)
                report["preflight"] = "passed"
                qualify(output, tests, environment, args.toolchain, git, crab)
                qualified = True
            finally:
                cleanup_error = None
                report["cleanup"] = "failed"
                try:
                    server.terminate()
                    try:
                        server.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait()
                    # S3 delete behavior is verified before and during the SDK cases.
                    # Tear down this private backend at the filesystem boundary so
                    # provider-specific recursive deletion cannot fail qualification.
                    shutil.rmtree(data)
                except Exception as error:
                    cleanup_error = error
                report["cleanup"] = "failed" if cleanup_error else "passed"
                if qualified and cleanup_error is None:
                    report["terminal_state"] = "passed"
                log.flush()
                contents = (work / "server.log").read_text(errors="replace")
                for name in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"):
                    contents = contents.replace(environment[name], "[redacted]")
                (output / "rustfs.log").write_text(contents)
                (output / "service.json").write_text(json.dumps(report, indent=2) + "\n")
                if cleanup_error is not None:
                    raise cleanup_error


if __name__ == "__main__":
    main()
