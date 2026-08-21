#!/usr/bin/env python3
"""Install Crab release binaries and the Git remote-helper symlink."""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import tempfile
from pathlib import Path


def install_binary(source: Path, destination: Path) -> None:
    if not source.is_file():
        raise FileNotFoundError(f"binary not found: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Rewriting a signed Mach-O in place can leave macOS with stale vnode
    # signature state. A same-directory rename publishes a complete new inode.
    with tempfile.NamedTemporaryFile(
        dir=destination.parent,
        prefix=f".{destination.name}.",
        delete=False,
    ) as temporary:
        temporary_path = Path(temporary.name)
    try:
        shutil.copy2(source, temporary_path)
        temporary_path.chmod(0o755)
        os.replace(temporary_path, destination)
    except OSError:
        temporary_path.unlink(missing_ok=True)
        raise


def install_symlink(target: str, link: Path) -> None:
    if link.exists() or link.is_symlink():
        if link.is_dir() and not link.is_symlink():
            raise IsADirectoryError(f"refusing to replace directory: {link}")
        link.unlink()
    link.symlink_to(target)


def install_nfs_mount_helper(source: Path, destination: Path, binary_name: str) -> None:
    if not source.exists() and not source.is_symlink():
        raise FileNotFoundError(f"NFS mount helper not found: {source}")
    if os.name == "nt":
        install_binary(source, destination)
    else:
        install_symlink(binary_name, destination)


def install_layout(
    prefix: Path,
    cargo_bin: Path,
    crab_bin: Path,
    cache_server_bin: Path,
    binary_name: str,
    cache_server_name: str,
    remote_link: str,
    nfs_mount_bin: Path | None,
    nfs_mount_name: str,
    fuse_mount_bin: Path | None,
    fuse_mount_name: str,
) -> None:
    prefix.mkdir(parents=True, exist_ok=True)

    install_binary(crab_bin, prefix / binary_name)
    install_binary(cache_server_bin, prefix / cache_server_name)
    if nfs_mount_bin is not None:
        install_nfs_mount_helper(nfs_mount_bin, prefix / nfs_mount_name, binary_name)
    if fuse_mount_bin is not None:
        install_binary(fuse_mount_bin, prefix / fuse_mount_name)
    install_symlink(binary_name, prefix / remote_link)
    print(f"installed {binary_name} -> {prefix / binary_name}")
    print(f"installed {cache_server_name} -> {prefix / cache_server_name}")
    if nfs_mount_bin is not None:
        print(f"installed {nfs_mount_name} -> {prefix / nfs_mount_name}")
    if fuse_mount_bin is not None:
        print(f"installed {fuse_mount_name} -> {prefix / fuse_mount_name}")
    print(f"symlinked {remote_link} -> {prefix / remote_link}")

    if str(prefix) != str(cargo_bin) and cargo_bin.is_dir():
        install_binary(crab_bin, cargo_bin / binary_name)
        install_binary(cache_server_bin, cargo_bin / cache_server_name)
        if nfs_mount_bin is not None:
            install_nfs_mount_helper(nfs_mount_bin, cargo_bin / nfs_mount_name, binary_name)
        if fuse_mount_bin is not None:
            install_binary(fuse_mount_bin, cargo_bin / fuse_mount_name)
        install_symlink(binary_name, cargo_bin / remote_link)
        print(f"installed {binary_name} -> {cargo_bin / binary_name}")
        print(f"installed {cache_server_name} -> {cargo_bin / cache_server_name}")
        if nfs_mount_bin is not None:
            print(f"installed {nfs_mount_name} -> {cargo_bin / nfs_mount_name}")
        if fuse_mount_bin is not None:
            print(f"installed {fuse_mount_name} -> {cargo_bin / fuse_mount_name}")
        print(f"symlinked {remote_link} -> {cargo_bin / remote_link}")


def main() -> int:
    parser = argparse.ArgumentParser(description="Install Crab binary layout.")
    parser.add_argument("--prefix", required=True, type=Path)
    parser.add_argument("--cargo-bin", required=True, type=Path)
    parser.add_argument("--crab-bin", required=True, type=Path)
    parser.add_argument("--cache-server-bin", required=True, type=Path)
    parser.add_argument("--binary-name", default="crab")
    parser.add_argument("--cache-server-name", default="crab-cache-server")
    parser.add_argument("--remote-link", default="git-remote-crab")
    parser.add_argument("--nfs-mount-bin", type=Path)
    parser.add_argument("--nfs-mount-name", default="crab-nfs-mount")
    parser.add_argument("--fuse-mount-bin", type=Path)
    parser.add_argument("--fuse-mount-name", default="crab-fuse-mount")
    args = parser.parse_args()

    try:
        install_layout(
            args.prefix,
            args.cargo_bin,
            args.crab_bin,
            args.cache_server_bin,
            args.binary_name,
            args.cache_server_name,
            args.remote_link,
            args.nfs_mount_bin,
            args.nfs_mount_name,
            args.fuse_mount_bin,
            args.fuse_mount_name,
        )
    except OSError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
