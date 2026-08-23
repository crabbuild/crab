#!/usr/bin/env python3
"""Verify the local installer layout without touching user bin dirs."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


CRAB_CLI_FEATURES_NO_FUSE = (
    "simd-accel,tier,replication-s3-control-plane,"
    "replication-gcs-control-plane,replication-azure-control-plane,"
    "coordinator-dynamodb,coordinator-spanner,coordinator-cosmosdb,watch,nfs,"
    "gix-pathmatch"
)
CRAB_CLI_FEATURES_WITH_FUSE = f"{CRAB_CLI_FEATURES_NO_FUSE},fuse"

INSTALLED_EXECUTABLES = (
    ("crab", "crab"),
    ("crab-cache-server", "crab-cache-server"),
    ("crab-nfs-mount", None),
)
FUSE_MOUNT_EXECUTABLE = ("crab-fuse-mount", None)
REMOTE_HELPER_LINK = "git-remote-crab"


def crab_dir() -> Path:
    return Path(__file__).resolve().parents[2]


def workspace_root() -> Path:
    return Path(__file__).resolve().parents[3]


def run(command: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )


def target_dir(cargo: str, root: Path) -> Path:
    result = run([cargo, "metadata", "--format-version", "1", "--no-deps"], root)
    if result.returncode != 0:
        raise RuntimeError(result.stdout)
    metadata = json.loads(result.stdout)
    return Path(metadata["target_directory"])


def binary_name(name: str) -> str:
    if sys.platform == "win32":
        return f"{name}.exe"
    return name


def installs_fuse_mount() -> bool:
    return sys.platform == "darwin" or sys.platform.startswith("linux")


def expected_executables() -> tuple[tuple[str, str | None], ...]:
    if installs_fuse_mount():
        return (*INSTALLED_EXECUTABLES, FUSE_MOUNT_EXECUTABLE)
    return INSTALLED_EXECUTABLES


def copy_debug_binary(artifacts: Path, source_name: str, destination_name: str) -> None:
    source = artifacts / binary_name(source_name)
    destination = artifacts / binary_name(destination_name)
    shutil.copy2(source, destination)
    destination.chmod(0o755)


def link_debug_binary(artifacts: Path, source_name: str, destination_name: str) -> None:
    source = binary_name(source_name)
    destination = artifacts / binary_name(destination_name)
    if destination.exists() or destination.is_symlink():
        destination.unlink()
    destination.symlink_to(source)


def stage_nfs_debug_binary(artifacts: Path) -> None:
    link_debug_binary(artifacts, "crab", "crab-nfs-mount")


def build_crab_binary(cargo: str, root: Path, features: str) -> subprocess.CompletedProcess[str]:
    return run(
        [
            cargo,
            "build",
            "-p",
            "crab",
            "--bin",
            "crab",
            "--no-default-features",
            "--features",
            features,
        ],
        root,
    )


def build_crab_nfs_mount_helper(cargo: str, root: Path) -> subprocess.CompletedProcess[str]:
    return run(
        [
            cargo,
            "build",
            "-p",
            "crab",
            "--bin",
            "crab-nfs-mount",
            "--no-default-features",
        ],
        root,
    )


def build_debug_binaries(cargo: str, root: Path) -> bool:
    try:
        artifacts = target_dir(cargo, root) / "debug"
    except RuntimeError as error:
        print("error: cargo metadata failed:", file=sys.stderr)
        print(error, file=sys.stderr)
        return False

    if installs_fuse_mount():
        fuse_result = build_crab_binary(cargo, root, CRAB_CLI_FEATURES_WITH_FUSE)
        if fuse_result.returncode != 0:
            print("error: FUSE-enabled debug binary build failed:", file=sys.stderr)
            print(result_command("crab", CRAB_CLI_FEATURES_WITH_FUSE), file=sys.stderr)
            print(fuse_result.stdout, file=sys.stderr)
            return False
        copy_debug_binary(artifacts, "crab", "crab-fuse-mount")

    nfs_result = build_crab_binary(cargo, root, CRAB_CLI_FEATURES_NO_FUSE)
    if nfs_result.returncode != 0:
        print("error: NFS-enabled debug binary build failed:", file=sys.stderr)
        print(result_command("crab", CRAB_CLI_FEATURES_NO_FUSE), file=sys.stderr)
        print(nfs_result.stdout, file=sys.stderr)
        return False
    if sys.platform == "win32":
        helper_result = build_crab_nfs_mount_helper(cargo, root)
        if helper_result.returncode != 0:
            print("error: NFS mount helper debug build failed:", file=sys.stderr)
            print(
                "cargo build -p crab --bin crab-nfs-mount --no-default-features",
                file=sys.stderr,
            )
            print(helper_result.stdout, file=sys.stderr)
            return False
    else:
        stage_nfs_debug_binary(artifacts)

    command = [
        cargo,
        "build",
        "-p",
        "crab-cache-server",
        "--bin",
        "crab-cache-server",
    ]
    result = run(command, root)
    if result.returncode == 0:
        return True

    print("error: debug binary build failed:", file=sys.stderr)
    print("  command:", " ".join(command), file=sys.stderr)
    print(result.stdout, file=sys.stderr)
    return False


def result_command(binary: str, features: str) -> str:
    return (
        f"cargo build -p crab --bin {binary} --no-default-features "
        f"--features {features}"
    )


def executable(path: Path) -> bool:
    return path.is_file() and os.access(path, os.X_OK)


def check_version(path: Path, binary_name: str, cwd: Path) -> str | None:
    result = run([str(path), "--version"], cwd)
    expected_prefix = f"{binary_name} "
    actual = result.stdout.strip()
    if result.returncode == 0 and actual.startswith(expected_prefix):
        return None
    detail = actual or result.stdout.strip()
    return (
        f"{binary_name}: expected `--version` output to start with "
        f"{expected_prefix!r}, got {detail!r}"
    )


def check_layout(
    prefix: Path,
    mirror: Path,
    cwd: Path,
    replaced_crab_identity: tuple[int, int],
) -> list[str]:
    errors: list[str] = []

    installed_crab = prefix / binary_name("crab")
    if installed_crab.is_file():
        installed_stat = installed_crab.stat()
        installed_identity = (installed_stat.st_dev, installed_stat.st_ino)
        if (
            installed_identity[1] != 0
            and replaced_crab_identity[1] != 0
            and installed_identity == replaced_crab_identity
        ):
            errors.append(
                "crab: installer rewrote the existing executable instead of replacing it"
            )

    for name, version_binary_name in expected_executables():
        path = prefix / name
        if not executable(path):
            errors.append(f"{name}: installed file is missing or not executable at {path}")
            continue
        if version_binary_name is not None:
            version_error = check_version(path, version_binary_name, cwd)
            if version_error is not None:
                errors.append(version_error)

    link = prefix / REMOTE_HELPER_LINK
    if not link.is_symlink():
        errors.append(f"{REMOTE_HELPER_LINK}: expected symlink at {link}")
    else:
        target = os.readlink(link)
        if target != "crab":
            errors.append(f"{REMOTE_HELPER_LINK}: expected symlink target 'crab', got {target!r}")
        elif link.resolve() != (prefix / "crab").resolve():
            errors.append(f"{REMOTE_HELPER_LINK}: symlink does not resolve to staged crab binary")

    if sys.platform != "win32":
        nfs_link = prefix / "crab-nfs-mount"
        if not nfs_link.is_symlink():
            errors.append(f"crab-nfs-mount: expected symlink at {nfs_link}")
        else:
            target = os.readlink(nfs_link)
            if target != "crab":
                errors.append(f"crab-nfs-mount: expected symlink target 'crab', got {target!r}")
            elif nfs_link.resolve() != (prefix / "crab").resolve():
                errors.append("crab-nfs-mount: symlink does not resolve to staged crab binary")

    if mirror.exists() and any(mirror.iterdir()):
        errors.append(f"disabled CARGO_BIN mirror path was unexpectedly populated: {mirror}")

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Stage the shared installer into a temporary prefix and verify binary layout.",
    )
    parser.add_argument("--cargo", default="cargo", help="Cargo executable to use for debug builds.")
    args = parser.parse_args()

    cwd = crab_dir()
    root = workspace_root()
    if not build_debug_binaries(args.cargo, root):
        return 1

    try:
        artifacts = target_dir(args.cargo, root) / "debug"
    except RuntimeError as error:
        print("error: cargo metadata failed:", file=sys.stderr)
        print(error, file=sys.stderr)
        return 1

    with tempfile.TemporaryDirectory(prefix="crab-install-layout-") as temp:
        temp_dir = Path(temp)
        prefix = temp_dir / "prefix"
        disabled_mirror = temp_dir / "cargo-bin-mirror-disabled"
        prefix.mkdir(parents=True)
        existing_crab = prefix / binary_name("crab")
        existing_crab.write_bytes(b"stale crab executable")
        existing_crab.chmod(0o755)
        existing_stat = existing_crab.stat()
        existing_identity = (existing_stat.st_dev, existing_stat.st_ino)
        installer = cwd / "scripts" / "release" / "install-binaries.py"
        command = [
            sys.executable,
            str(installer),
            "--prefix",
            str(prefix),
            "--cargo-bin",
            str(disabled_mirror),
            "--crab-bin",
            str(artifacts / binary_name("crab")),
            "--cache-server-bin",
            str(artifacts / binary_name("crab-cache-server")),
            "--nfs-mount-bin",
            str(artifacts / binary_name("crab-nfs-mount")),
        ]
        if installs_fuse_mount():
            command.extend(
                [
                    "--fuse-mount-bin",
                    str(artifacts / binary_name("crab-fuse-mount")),
                ]
            )
        result = run(command, cwd)
        if result.returncode != 0:
            print("error: staged install failed:", file=sys.stderr)
            print("  command:", " ".join(command), file=sys.stderr)
            print(result.stdout, file=sys.stderr)
            return 1

        errors = check_layout(prefix, disabled_mirror, cwd, existing_identity)
        if errors:
            print("error: staged install layout drifted:", file=sys.stderr)
            print("  command:", " ".join(command), file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
            print(result.stdout, file=sys.stderr)
            return 1

        installed = ", ".join(
            [name for name, _version_binary_name in expected_executables()]
            + [REMOTE_HELPER_LINK]
        )
        print(f"ok: staged installer produced expected layout: {installed}")
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
