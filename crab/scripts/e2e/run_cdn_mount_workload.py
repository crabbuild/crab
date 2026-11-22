#!/usr/bin/env python3
"""Read an approved projected Xet file through an ordinary Crab NFS mount."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from collect_cdn_product_metrics import NATIVE_TARGET, TARGET, measure, measure_native
from cdn_qualification import (
    CDN_PROVENANCE_FIELDS, CLI_BUILD_FIELDS, COMMIT, RUN_NUMBER, decode, fields, is_direct_mount_xorb_scope,
    MOUNT_XORB_RANGE_READ, matches, read_git_record, require, scope_parts, validate_xorb_publication,
    validate_xorb_workload, verify_read_candidate, verify_sparse_xorb_traffic, verify_xorb_read,
)
from run_cdn_git_workload import (
    PAYLOAD, companion_crab, git_environment, read_target_fingerprints, runner_platform,
)
from run_cdn_sdk_workload import binding_for, json_record
from run_cdn_transport_checks import approved_fixture, fixture_object_key, native_remote


MOUNT_TARGET = "crab::cdn::qualification_mount"
MOUNT_MESSAGE = "CDN qualification verified mount read configuration"
MAX_TRACE_BYTES = 16 * 1024 * 1024
TIMEOUT = 300

READ_FILE = """
import hashlib, json, os, stat, sys
path, size, offset, length = sys.argv[1], *map(int, sys.argv[2:])
assert stat.S_ISREG(os.lstat(path).st_mode)
fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_BINARY", 0))
with os.fdopen(fd, "rb", buffering=0) as stream:
    info = os.fstat(stream.fileno())
    assert stat.S_ISREG(info.st_mode) and info.st_size == size
    assert 0 <= offset and 0 < length and offset + length <= size
    stream.seek(offset)
    digest = hashlib.sha256()
    remaining = length
    while remaining:
        data = stream.read(min(1024 * 1024, remaining))
        assert data
        digest.update(data)
        remaining -= len(data)
    if length == size:
        assert not stream.read(1)
print(json.dumps({"sha256": digest.hexdigest(), "bytes": length}))
"""


def sha256(value: str | bytes) -> str:
    return hashlib.sha256(value.encode() if isinstance(value, str) else value).hexdigest()


def command(arguments: list[str], env: dict[str, str], directory: Path, *, timeout: float = 30) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(arguments, env=env, cwd=directory, capture_output=True, timeout=timeout, check=False)
    except subprocess.TimeoutExpired:
        raise ValueError("mount qualification command timed out") from None


def make_home(home: Path, binding: dict, fixture: dict, arm: str) -> None:
    config = home / ".config/crab"
    config.mkdir(parents=True)
    # File-key runners reuse their dedicated login without keeping any keys in evidence.
    source_key = Path.home() / ".config/crab/.token-key"
    if sys.platform != "darwin" or os.environ.get("CRAB_NO_KEYCHAIN", "").lower() in {"1", "true"}:
        key = read_git_record(source_key, limit=32)
        require(len(key) == 32, "mount qualification requires the dedicated worker's file-based login key")
        with os.fdopen(os.open(config / ".token-key", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as stream:
            stream.write(key)

    auth = {
        "provider": "crab-auth", "auth_endpoint": binding["endpoint"], "issuer_url": binding["oidc_issuer"],
        "client_id": binding["oidc_client_id"], "token_cache_path": binding["token_cache_directory"],
    }
    parts = [("[auth]", auth)]
    if arm == "edge":
        parts.append(("[[cdn]]", {
            "origin": fixture["native_origin"], "repo_prefix": fixture["fixture_prefix"],
            "base_url": fixture["base_url"], "approval_sha256": binding["approval_sha256"],
        }))
    with (config / "config.toml").open("x", encoding="utf-8") as stream:
        for section, values in parts:
            stream.write(section + "\n")
            for name, value in values.items():
                stream.write(f"{name} = {json.dumps(value, ensure_ascii=False)}\n")


def status(crab: Path, mountpoint: Path, root: Path, env: dict[str, str]) -> subprocess.CompletedProcess:
    return command([str(crab), "mount", "status", "--mountpoint", str(mountpoint), "--json", "--live-only"],
                   env, root, timeout=8)


def confirm_status(value: dict, pid: int, remote: str, ref: str, expected_oid: str) -> None:
    require(isinstance(value, dict) and value.get("pid") == pid
            and value.get("backend") == "nfs" and value.get("read_only") is True
            and value.get("source") == remote and value.get("ref") == ref.removeprefix("refs/heads/")
            and value.get("head_oid") == expected_oid,
            "live NFS session did not confirm the approved read-only repository, ref and commit")


def read_mounted_file(path: Path, spec: dict, env: dict[str, str], root: Path) -> None:
    window = spec.get("read_range")
    if window is None:
        window = {"offset": 0, "bytes": spec["payload_bytes"], "sha256": spec["payload_sha256"]}
    arguments = [str(path), str(spec["payload_bytes"]), str(window["offset"]), str(window["bytes"])]
    read = command([sys.executable, "-I", "-c", READ_FILE, *arguments], env, root, timeout=TIMEOUT)
    require(read.returncode == 0 and decode(read.stdout) == {name: window[name] for name in ("sha256", "bytes")},
            "mounted filesystem returned different payload bytes")


def run_mount(
    crab: Path, remote: str, ref: str, mountpoint: Path, root: Path, env: dict[str, str],
    spec: dict,
) -> tuple[dict, bytes, list[bytes], list[bytes]]:
    require(not os.path.ismount(mountpoint), "measurement mountpoint is already mounted")
    with tempfile.TemporaryFile(dir=root) as trace:
        started = time.time_ns() // 1_000_000
        timer = time.monotonic_ns()
        process = subprocess.Popen(
            [str(crab), "mount", "--repo", remote, "--ref", ref, "--mountpoint", str(mountpoint),
             "--backend", "nfs", "--foreground", "--read-only", "--no-refresh"],
            cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=trace,
        )
        deadline = time.monotonic() + TIMEOUT
        active = False
        stop = [str(crab), "unmount", "--mountpoint", str(mountpoint)]
        try:
            while time.monotonic() < deadline and process.poll() is None:
                live = status(crab, mountpoint, root, env)
                if live.returncode == 0:
                    confirm_status(decode(live.stdout), process.pid, remote, ref, spec["mounted_oid"])
                    active = True
                    break
                time.sleep(0.25)
            require(active and os.path.ismount(mountpoint), "ordinary Crab NFS mount did not become live")

            # Inspect the Git pointer from this mount's own clone before timing xorb traffic.
            git_dir = Path(env["HOME"]) / ".crab/mounts/repos" / sha256(remote)[:12] / ".git"
            git = ["git", f"--git-dir={git_dir}", "cat-file"]
            blob = f"{spec['mounted_oid']}:{PAYLOAD}"
            size = command(git + ["-s", blob], env, root)
            require(size.returncode == 0 and size.stdout.strip().isdigit() and 0 < int(size.stdout) <= 4096,
                    "mounted Git commit does not contain a bounded Xet pointer")
            pointer = command(git + ["blob", blob], env, root)
            require(pointer.returncode == 0 and 0 < len(pointer.stdout) <= 4096
                    and sha256(pointer.stdout) == spec["pointer_sha256"],
                    "mounted Git commit does not contain the approved Xet pointer")
            first = os.fstat(trace.fileno()).st_size
            read_mounted_file(mountpoint / PAYLOAD, spec, env, root)
            last = os.fstat(trace.fileno()).st_size
            live = status(crab, mountpoint, root, env)
            require(live.returncode == 0, "mount closed before the filesystem read was confirmed")
            confirm_status(decode(live.stdout), process.pid, remote, ref, spec["mounted_oid"])
            require(command(stop, env, root).returncode == 0, "ordinary Crab unmount failed")
            require(process.wait(timeout=30) == 0 and not os.path.ismount(mountpoint)
                    and status(crab, mountpoint, root, env).returncode != 0, "NFS session did not detach cleanly")
            if "read_range" in spec:
                # Count read-ahead and cancelled requests that finish during detach.
                last = os.fstat(trace.fileno()).st_size
            require(0 <= first <= last <= MAX_TRACE_BYTES, "mounted read exceeded the redacted trace limit")
            finish = time.time_ns() // 1_000_000
            elapsed = (time.monotonic_ns() - timer + 999_999) // 1_000_000
            trace.seek(0)
            startup = trace.read(first).splitlines()
            transfers = trace.read(last - first).splitlines()
            return {"started_at_unix_ms": started, "finished_at_unix_ms": finish, "elapsed_ms": elapsed}, pointer.stdout, startup, transfers
        finally:
            if process.poll() is None:
                # The ordinary command drains the native session. SIGINT lets
                # Unix clean up a failed startup that never registered a mount.
                if active or os.path.ismount(mountpoint):
                    try:
                        command(stop, env, root, timeout=15)
                    except (OSError, ValueError):
                        pass
                elif os.name != "nt":
                    process.send_signal(signal.SIGINT)
                else:
                    process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def mount_record(startup: list[bytes], transfers: list[bytes], fixture: dict, candidate: dict,
                 reader: dict, key_digest: str, required_xorb_source: str | None) -> dict:
    target = {name: reader[name] for name in (
        "origin_sha256", "prefix_sha256", "global_prefix_sha256", "binding_sha256", "cache_root_sha256",
    )}
    target |= {key: candidate[key] for key in CLI_BUILD_FIELDS}
    target |= {"message": MOUNT_MESSAGE, "public_export_sha256": ""}
    matches_target = []
    for line in startup:
        if MOUNT_TARGET.encode() not in line:
            continue
        value = decode(line)
        if isinstance(value, dict) and value.get("target") == MOUNT_TARGET:
            matches_target.append(value.get("fields"))
    require(matches_target == [target], "mount did not confirm its approved native target, build, binding and cache")

    responses: dict = {}
    counts = measure(transfers, fixture["scope"], expected_xorb_key=key_digest, edge_responses=responses,
                     required_xorb_source=required_xorb_source)
    return counts | {"reader": reader, "xorb_key_sha256": key_digest, "edge_responses": responses,
                     "native_traffic": measure_native(transfers, fixture["scope"])}


def workload(args: argparse.Namespace) -> tuple[dict, dict | None]:
    require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
            and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
    require(args.warm_local_output is None or args.arm == "edge", "a warm local remount requires its own edge primer")
    fixture = approved_fixture(args.fixture, args.configuration_sha256)
    scope = fixture["scope"]
    require(is_direct_mount_xorb_scope(scope) and fixture["view_suffix"],
            "mount evidence requires a private direct projected xorb fixture")
    edge, access, _, family, _ = scope_parts(scope)
    key = fixture_object_key(fixture, access, family)
    key_digest = sha256(key)
    physical = fixture["fixture_prefix"] + fixture["view_suffix"]
    spec = validate_xorb_workload(read_git_record(args.workload), args.workload_sha256,
                                  args.configuration_sha256, mount=True, allow_sparse=True)
    window = spec.get("read_range")
    require(window is None or args.warm_local_output is None, "sparse mount uses separate cold and warm edge sessions")
    require(spec["global_prefix_sha256"] == sha256(key.rsplit("/xorbs/", 1)[0]),
            "workload does not name the projected xorb namespace")
    platform = runner_platform()
    row = {"scope": scope, "configuration_sha256": args.configuration_sha256, "source_sha": args.source_sha,
           "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt, "platform": platform}
    publication_raw = read_git_record(args.receipt)
    publication = validate_xorb_publication(publication_raw, row)
    require(publication["published"] and sha256(publication_raw) == spec["publication_sha256"]
            and publication["sha256"] == spec["pointer_sha256"],
            "mount workload requires the independently published pointer")
    candidate = verify_read_candidate(publication["candidate"], args.source_sha, platform)
    crab = companion_crab(args.helper)
    with crab.open("rb") as binary:
        require(hashlib.file_digest(binary, "sha256").hexdigest() == candidate["binary_sha256"] == spec["crab_sha256"],
                "mount must use the approved Crab executable")

    args.export_approval = None
    binding, approval, _ = binding_for(args, fixture)
    require((args.drive is not None) == (os.name == "nt"), "Windows NFS needs a dedicated unused drive letter")
    if args.drive is not None:
        require(re.fullmatch(r"[A-Za-z]:", args.drive) is not None, "invalid NFS measurement drive")
    root = Path(tempfile.mkdtemp(prefix="crab-cdn-mount-", dir=args.scratch.resolve(strict=True))).resolve()
    mountpoint = Path(args.drive + "\\") if args.drive is not None else root / "mounted"
    if args.drive is not None and mountpoint.exists():
        shutil.rmtree(root)
        raise ValueError("NFS measurement drive is in use")
    try:
        if args.drive is None:
            mountpoint.mkdir()
        cache = root / "cache"
        process_directory = root / "process"
        process_directory.mkdir()
        reader = read_target_fingerprints(fixture | {"fixture_prefix": physical}, edge=args.arm == "edge", export_sha256="")
        reader.pop("public_export_sha256")
        reader |= {
            "global_prefix_sha256": spec["global_prefix_sha256"], "approval_sha256": approval,
            "cache_root_sha256": sha256(str(cache)), "local_cache_only": True,
        }
        target = {"origin": fixture["native_origin"], "repository_prefix": physical, "read": {
            "binding_sha256": reader["binding_sha256"], "cache_root_sha256": reader["cache_root_sha256"],
            "public_export_sha256": "", "mount_global_prefix_sha256": reader["global_prefix_sha256"],
        }}
        env = git_environment(args.helper, root, target)
        env = {name: value for name, value in env.items()
               if not name.upper().startswith(("CRAB_AUTH_", "CRAB_CACHE_", "CRAB_CDN_"))
               and name not in ("CRAB_OTLP_ENDPOINT", "CRAB_PROFILE")}
        remote = native_remote(fixture, edge)
        registration = {"hydration_workload_sha256": args.workload_sha256, "candidate": candidate,
                        "viewer_binding_sha256": reader["binding_sha256"], "viewer_approval_sha256": approval}
        placement = {"origin_sha256": reader["origin_sha256"],
                     "views": {"target": {"physical_prefix_sha256": reader["prefix_sha256"], "key_sha256": key_digest}}}
        context = {name: row[name] for name in CDN_PROVENANCE_FIELDS | {"platform"}} | {
            "schema": "crab.cdn-mount-xorb-read/v1", "workload_sha256": args.workload_sha256, "candidate": candidate,
            "crab_sha256": spec["crab_sha256"], "mounted_oid": spec["mounted_oid"], "unmounted": True,
            "payload": {"sha256": spec["payload_sha256"], "bytes": spec["payload_bytes"]},
        }
        if window is not None:
            context |= {"schema": MOUNT_XORB_RANGE_READ, "range_offset": window["offset"],
                        "payload": {name: window[name] for name in ("sha256", "bytes")}}

        def arm(name: str, primer: dict | None = None) -> dict:
            # The local arm reuses only Crab's cache. A new mount and Git
            # metadata home prevent a kernel hit from qualifying the arm.
            home = root / name
            make_home(home, binding, fixture, args.arm)
            arm_env = env | {"HOME": str(home), "USERPROFILE": str(home), "CRAB_CACHE_DIR": str(cache),
                             "CRAB_LOG": f"{MOUNT_TARGET}=debug,{NATIVE_TARGET}=debug,{TARGET}=debug,crab_storage::cdn::gcs_viewer_log=trace"}
            timing, pointer, startup, transfers = run_mount(
                crab, remote, publication["ref"], mountpoint, process_directory, arm_env, spec,
            )
            source = "local" if name == "warm_local" else None
            if window is not None and name == "edge":
                source = "edge"
            evidence = mount_record(startup, transfers, fixture, candidate, reader, key_digest, source)
            result = context | timing | evidence | {"arm": name, "pointer_sha256": sha256(pointer)}
            if primer is not None:
                result["primer_sha256"] = sha256(json_record(primer))
            verified = verify_xorb_read(json_record(result).encode(), row, registration, name, spec, placement)
            if window is not None and name == "edge":
                verify_sparse_xorb_traffic(verified, fixture["size"])
            return verified

        first = arm(args.arm)
        return first, arm("warm_local", first) if args.warm_local_output is not None else None
    finally:
        # Never traverse a mount during failure cleanup.
        for name in (args.arm, "warm_local"):
            (root / name / ".config/crab/.token-key").unlink(missing_ok=True)
        if not os.path.ismount(mountpoint):
            if args.drive is None:
                mountpoint.rmdir()
            shutil.rmtree(root)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("arm", choices=("edge", "origin"))
    for name in ("fixture", "workload", "receipt", "helper", "private-reader", "scratch"):
        parser.add_argument("--" + name, required=True, type=Path)
    for name in ("configuration-sha256", "workload-sha256", "source-sha", "run-id", "run-attempt"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--warm-local-output", type=Path)
    parser.add_argument("--drive")
    args = parser.parse_args()
    try:
        primary, local = workload(args)
        if local is not None:
            with args.warm_local_output.open("x", encoding="utf-8", newline="\n") as stream:
                stream.write(json_record(local))
        sys.stdout.write(json_record(primary))
    except (OSError, ValueError, subprocess.TimeoutExpired):
        # Product diagnostics may contain private fixture settings.
        print("error: mounted CDN evidence could not be verified", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
