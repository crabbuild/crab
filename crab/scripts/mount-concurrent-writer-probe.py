#!/usr/bin/env python3
"""Create and verify deterministic concurrent mount writes."""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import subprocess
import sys
import tempfile
from typing import Optional


WORKER = r"""
import hashlib
import json
import os
import pathlib
import sys
import time

index = int(sys.argv[1])
size = int(sys.argv[2])
path = pathlib.Path(sys.argv[3])
started_ns = time.monotonic_ns()
digest = hashlib.sha256()
remaining = size
counter = 0
with path.open("wb", buffering=0) as output:
    while remaining:
        wanted = min(1024 * 1024, remaining)
        data = bytearray()
        while len(data) < wanted:
            data.extend(hashlib.sha256(f"concurrent-{index}:{counter}".encode()).digest())
            counter += 1
        block = bytes(data[:wanted])
        output.write(block)
        digest.update(block)
        remaining -= wanted
    os.fsync(output.fileno())
ended_ns = time.monotonic_ns()
print(json.dumps({
    "index": index,
    "path": path.name,
    "size_bytes": size,
    "sha256": digest.hexdigest(),
    "started_ns": started_ns,
    "ended_ns": ended_ns,
}))
"""


def load_manifest(path: pathlib.Path) -> dict:
    return json.loads(path.read_text())


def file_hash(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def write_files(models: pathlib.Path, writers: int, mib: int, output: pathlib.Path) -> None:
    size_bytes = mib * 1024 * 1024
    processes = [
        subprocess.Popen(
            [
                sys.executable,
                "-c",
                WORKER,
                str(index),
                str(size_bytes),
                str(models / f"concurrent-{index:02}.bin"),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        for index in range(writers)
    ]
    results = []
    for process in processes:
        stdout, stderr = process.communicate()
        if process.returncode != 0:
            raise RuntimeError(stderr or f"concurrent writer exited {process.returncode}")
        results.append(json.loads(stdout))

    overlap_ns = min(item["ended_ns"] for item in results) - max(
        item["started_ns"] for item in results
    )
    if overlap_ns <= 0:
        raise RuntimeError("concurrent writer intervals did not overlap")
    elapsed_ns = max(item["ended_ns"] for item in results) - min(
        item["started_ns"] for item in results
    )
    logical_bytes = writers * size_bytes
    payload = {
        "writer_count": writers,
        "bytes_per_writer": size_bytes,
        "logical_bytes": logical_bytes,
        "elapsed_ms": elapsed_ns // 1_000_000,
        "overlap_ms": overlap_ns // 1_000_000,
        "throughput_mib_per_second": logical_bytes
        / (1024 * 1024)
        / (elapsed_ns / 1e9),
        "files": sorted(results, key=lambda item: item["index"]),
    }
    output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")


def verify_files(manifest: pathlib.Path, models: pathlib.Path, label: str) -> None:
    for item in load_manifest(manifest)["files"]:
        digest = file_hash(models / item["path"])
        if digest != item["sha256"]:
            raise RuntimeError(f"{label} hash mismatch for {item['path']}: {digest}")


def verify_pointers(
    manifest: pathlib.Path,
    *,
    models: Optional[pathlib.Path],
    repo: Optional[pathlib.Path],
) -> None:
    for item in load_manifest(manifest)["files"]:
        if repo is not None:
            pointer = subprocess.check_output(
                ["git", "show", f"HEAD:models/{item['path']}"], cwd=repo, text=True
            )
        else:
            if models is None:
                raise RuntimeError("models path is required")
            pointer = (models / item["path"]).read_text()
        if "version https://crab.build/spec/v1" not in pointer or len(pointer) >= 1000:
            raise RuntimeError(f"invalid pointer for {item['path']}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    write = subparsers.add_parser("write")
    write.add_argument("--models", type=pathlib.Path, required=True)
    write.add_argument("--writers", type=int, required=True)
    write.add_argument("--mib", type=int, required=True)
    write.add_argument("--output", type=pathlib.Path, required=True)

    verify = subparsers.add_parser("verify-files")
    verify.add_argument("--manifest", type=pathlib.Path, required=True)
    verify.add_argument("--models", type=pathlib.Path, required=True)
    verify.add_argument("--label", required=True)

    pointers = subparsers.add_parser("verify-pointers")
    pointers.add_argument("--manifest", type=pathlib.Path, required=True)
    target = pointers.add_mutually_exclusive_group(required=True)
    target.add_argument("--models", type=pathlib.Path)
    target.add_argument("--repo", type=pathlib.Path)

    subparsers.add_parser("self-test")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.command == "write":
        write_files(args.models, args.writers, args.mib, args.output)
    elif args.command == "verify-files":
        verify_files(args.manifest, args.models, args.label)
    elif args.command == "verify-pointers":
        verify_pointers(args.manifest, models=args.models, repo=args.repo)
    else:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            manifest = root / "manifest.json"
            write_files(root, 2, 1, manifest)
            verify_files(manifest, root, "self-test")


if __name__ == "__main__":
    main()
