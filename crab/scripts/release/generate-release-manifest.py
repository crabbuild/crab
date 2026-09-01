#!/usr/bin/env python3
"""Generate the bounded, deterministic public Crab release manifest."""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import re
import sys
import tempfile


SCHEMA = "crab.release/1"
MANIFEST_NAME = "crab-release.json"
MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
ARTIFACTS = (
    ("aarch64-apple-darwin", "crab-darwin-aarch64.tar.gz"),
    ("aarch64-pc-windows-msvc", "crab-windows-aarch64.zip"),
    ("aarch64-unknown-linux-gnu", "crab-linux-aarch64.tar.gz"),
    ("x86_64-apple-darwin", "crab-darwin-x86_64.tar.gz"),
    ("x86_64-pc-windows-msvc", "crab-windows-x86_64.zip"),
    ("x86_64-unknown-linux-gnu", "crab-linux-x86_64.tar.gz"),
)
TAG_PATTERN = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def generate(tag: str, dist: pathlib.Path) -> dict[str, object]:
    if TAG_PATTERN.fullmatch(tag) is None:
        raise ValueError(f"invalid stable Crab release tag: {tag}")
    artifacts: list[dict[str, object]] = []
    for target, archive_name in ARTIFACTS:
        archive = dist / archive_name
        if not archive.is_file():
            raise ValueError(f"release is missing archive for {target}: {archive_name}")
        size = archive.stat().st_size
        if size <= 0 or size > MAX_ARCHIVE_BYTES:
            raise ValueError(f"release archive has invalid size for {target}: {size}")
        artifacts.append(
            {
                "target": target,
                "archive": archive_name,
                "sha256": sha256(archive),
                "bytes": size,
            }
        )
    return {
        "schema": SCHEMA,
        "version": tag.removeprefix("v"),
        "tag": tag,
        "artifacts": artifacts,
    }


def write_manifest(tag: str, dist: pathlib.Path) -> pathlib.Path:
    manifest = generate(tag, dist)
    destination = dist / MANIFEST_NAME
    temporary_path: pathlib.Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=dist,
            prefix=f".{MANIFEST_NAME}.",
            delete=False,
        ) as stream:
            temporary_path = pathlib.Path(stream.name)
            json.dump(manifest, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary_path, destination)
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
    return destination


def self_test() -> None:
    with tempfile.TemporaryDirectory() as directory:
        dist = pathlib.Path(directory)
        for target, archive_name in ARTIFACTS:
            (dist / archive_name).write_bytes(f"archive for {target}\n".encode())
        first = write_manifest("v1.2.3", dist).read_bytes()
        manifest = json.loads(first)
        assert manifest["schema"] == SCHEMA
        assert manifest["version"] == "1.2.3"
        assert manifest["tag"] == "v1.2.3"
        assert [artifact["target"] for artifact in manifest["artifacts"]] == [
            target for target, _ in ARTIFACTS
        ]
        for artifact in manifest["artifacts"]:
            archive = dist / artifact["archive"]
            assert artifact["sha256"] == sha256(archive)
            assert artifact["bytes"] == archive.stat().st_size
        assert write_manifest("v1.2.3", dist).read_bytes() == first

        (dist / ARTIFACTS[0][1]).unlink()
        try:
            generate("v1.2.3", dist)
        except ValueError:
            pass
        else:
            raise AssertionError("missing release archive was accepted")

        try:
            generate("v1.2.3-beta.1", dist)
        except ValueError:
            pass
        else:
            raise AssertionError("prerelease tag was accepted")


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return 0
    if len(sys.argv) != 3:
        print(
            "usage: generate-release-manifest.py <vVERSION> <dist-directory>",
            file=sys.stderr,
        )
        return 2
    dist = pathlib.Path(sys.argv[2])
    if not dist.is_dir():
        print(f"error: release directory does not exist: {dist}", file=sys.stderr)
        return 2
    try:
        destination = write_manifest(sys.argv[1], dist)
    except (OSError, UnicodeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(destination)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
