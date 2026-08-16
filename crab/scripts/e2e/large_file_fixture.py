#!/usr/bin/env python3
"""Deterministic large-file fixtures for Crab end-to-end scale runs.

The module is intentionally stdlib-only so it can run on a fresh dev
machine. File contents are streamed in bounded chunks and every manifest
entry records the hash Crab workflow checks use later.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import shutil
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import Iterable


MIB = 1024 * 1024
GIB = 1024 * MIB
DEFAULT_CHUNK_SIZE = 8 * MIB
DEFAULT_SEED = 0xC0A6_E2E


@dataclass(frozen=True)
class FileSpec:
    path: str
    size: int
    seed: int


PROFILES: dict[str, list[FileSpec]] = {
    "tiny": [
        FileSpec("data/model-000.bin", 8 * MIB, 0x1000),
        FileSpec("data/model-001.bin", 10 * MIB, 0x1001),
        FileSpec("data/model-002.bin", 12 * MIB, 0x1002),
        FileSpec("data/model-003.bin", 14 * MIB, 0x1003),
    ],
    "smoke": [
        FileSpec("data/model-000.bin", 100 * MIB, 0x2000),
        FileSpec("data/model-001.bin", 512 * MIB, 0x2001),
        FileSpec("data/model-002.bin", 1 * GIB, 0x2002),
        FileSpec("data/model-003.bin", 3 * GIB, 0x2003),
        FileSpec("data/model-004.bin", 5 * GIB, 0x2004),
    ],
    "battle": [
        *[
            FileSpec(f"data/model-{idx:03}.bin", 5 * GIB, 0x3000 + idx)
            for idx in range(8)
        ],
        *[
            FileSpec(f"data/checkpoint-{idx:03}.bin", 2 * GIB, 0x3100 + idx)
            for idx in range(4)
        ],
        *[
            FileSpec(f"data/features-{idx:03}.bin", 1 * GIB, 0x3200 + idx)
            for idx in range(2)
        ],
    ],
    "max": [
        *[
            FileSpec(f"data/model-{idx:03}.bin", 5 * GIB, 0x4000 + idx)
            for idx in range(18)
        ],
        *[
            FileSpec(f"data/checkpoint-{idx:03}.bin", 2 * GIB, 0x4100 + idx)
            for idx in range(4)
        ],
        *[
            FileSpec(f"data/features-{idx:03}.bin", 1 * GIB, 0x4200 + idx)
            for idx in range(2)
        ],
    ],
}


class FixtureError(RuntimeError):
    """Raised when fixture generation or mutation cannot continue."""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def profile_bytes(profile: str) -> int:
    return sum(spec.size for spec in PROFILES[profile])


def relpath(path: Path, root: Path) -> str:
    return path.relative_to(root).as_posix()


def safe_join(root: Path, relative: str) -> Path:
    pure = PurePosixPath(relative)
    if pure.is_absolute() or ".." in pure.parts:
        raise FixtureError(f"unsafe relative path: {relative}")
    return root.joinpath(*pure.parts)


def deterministic_rng(path: str, version: int, seed: int) -> random.Random:
    material = f"{DEFAULT_SEED}:{seed}:{version}:{path}".encode()
    digest = hashlib.blake2b(material, digest_size=16).digest()
    return random.Random(int.from_bytes(digest, "big"))


def write_deterministic_file(
    root: Path,
    relative: str,
    size: int,
    *,
    version: int,
    seed: int,
    chunk_size: int = DEFAULT_CHUNK_SIZE,
) -> dict:
    path = safe_join(root, relative)
    path.parent.mkdir(parents=True, exist_ok=True)
    rng = deterministic_rng(relative, version, seed)
    sha = hashlib.sha256()
    written = 0

    with path.open("wb") as fh:
        while written < size:
            wanted = min(chunk_size, size - written)
            data = rng.randbytes(wanted)
            fh.write(data)
            sha.update(data)
            written += wanted

    return {
        "path": relative,
        "version": version,
        "size": size,
        "sha256": sha.hexdigest(),
        "seed": seed,
        "operations": [{"op": "create", "bytes": size}],
    }


def hash_file(path: Path, *, chunk_size: int = DEFAULT_CHUNK_SIZE) -> tuple[int, str]:
    sha = hashlib.sha256()
    size = 0
    with path.open("rb") as fh:
        while True:
            chunk = fh.read(chunk_size)
            if not chunk:
                break
            size += len(chunk)
            sha.update(chunk)
    return size, sha.hexdigest()


def manifest_entries(manifest: dict) -> dict[str, dict]:
    return {entry["path"]: entry for entry in manifest.get("files", [])}


def build_manifest(
    *,
    profile: str,
    root: Path,
    files: Iterable[dict],
    mutation: str | None = None,
    changed_paths: Iterable[str] | None = None,
) -> dict:
    entries = sorted(files, key=lambda item: item["path"])
    return {
        "schema": "crab.large-file-fixture",
        "schema_version": 1,
        "profile": profile,
        "root": str(root),
        "created_at": utc_now(),
        "mutation": mutation,
        "changed_paths": sorted(changed_paths or []),
        "total_bytes": sum(
            int(entry["size"]) for entry in entries if not entry.get("deleted")
        ),
        "files": entries,
    }


def write_manifest(path: Path, manifest: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")


def load_manifest(path: Path) -> dict:
    return json.loads(path.read_text())


def create_profile(root: Path, profile: str, manifest_path: Path) -> dict:
    if profile not in PROFILES:
        raise FixtureError(f"unknown profile {profile!r}")
    root.mkdir(parents=True, exist_ok=True)

    files = [
        write_deterministic_file(
            root,
            spec.path,
            spec.size,
            version=1,
            seed=spec.seed,
        )
        for spec in PROFILES[profile]
    ]
    manifest = build_manifest(profile=profile, root=root, files=files)
    write_manifest(manifest_path, manifest)
    return manifest


def patch_bytes(label: str, length: int) -> bytes:
    digest = hashlib.blake2b(label.encode(), digest_size=16).digest()
    rng = random.Random(int.from_bytes(digest, "big"))
    return rng.randbytes(length)


def overwrite_range(path: Path, offset: int, data: bytes) -> dict:
    with path.open("r+b") as fh:
        fh.seek(offset)
        fh.write(data)
    return {"op": "overwrite", "offset": offset, "bytes": len(data)}


def append_range(path: Path, data: bytes) -> dict:
    with path.open("ab") as fh:
        fh.write(data)
    return {"op": "append", "bytes": len(data)}


def update_entry(root: Path, entry: dict, operations: list[dict], *, version_bump: int = 1) -> dict:
    path = safe_join(root, entry["path"])
    size, digest = hash_file(path)
    updated = dict(entry)
    updated["version"] = int(updated.get("version", 1)) + version_bump
    updated["size"] = size
    updated["sha256"] = digest
    updated["operations"] = list(updated.get("operations", [])) + operations
    return updated


def rewrite_small_delta(root: Path, entry: dict, label: str, *, span: int = 4 * MIB) -> dict:
    path = safe_join(root, entry["path"])
    size = path.stat().st_size
    if size == 0:
        raise FixtureError(f"cannot delta empty file {entry['path']}")
    length = min(span, max(1, size // 16))
    max_offset = max(0, size - length)
    offset = min(max_offset, max(0, size // 3))
    op = overwrite_range(path, offset, patch_bytes(f"{label}:overwrite", length))
    return update_entry(root, entry, [op])


def append_small_delta(root: Path, entry: dict, label: str, *, span: int = 2 * MIB) -> dict:
    path = safe_join(root, entry["path"])
    op = append_range(path, patch_bytes(f"{label}:append", span))
    return update_entry(root, entry, [op])


def rename_entry(root: Path, entry: dict, new_path: str, label: str) -> dict:
    src = safe_join(root, entry["path"])
    dst = safe_join(root, new_path)
    dst.parent.mkdir(parents=True, exist_ok=True)
    src.rename(dst)
    updated = dict(entry)
    updated["path"] = new_path
    updated["version"] = int(updated.get("version", 1)) + 1
    updated["operations"] = list(updated.get("operations", [])) + [
        {"op": "rename", "from": entry["path"], "to": new_path, "label": label}
    ]
    return updated


def delete_entry(root: Path, entry: dict, label: str) -> dict:
    path = safe_join(root, entry["path"])
    path.unlink(missing_ok=True)
    deleted = dict(entry)
    deleted["deleted"] = True
    deleted["version"] = int(deleted.get("version", 1)) + 1
    deleted["operations"] = list(deleted.get("operations", [])) + [
        {"op": "delete", "label": label}
    ]
    return deleted


def add_new_entry(root: Path, relative: str, size: int, seed: int, label: str) -> dict:
    entry = write_deterministic_file(root, relative, size, version=1, seed=seed)
    entry["operations"] = [{"op": "add", "label": label, "bytes": size}]
    return entry


def first_existing(files: dict[str, dict], preferred: list[str], fallback_index: int = 0) -> dict:
    for path in preferred:
        if path in files:
            return files[path]
    existing = [entry for entry in files.values() if not entry.get("deleted")]
    if not existing:
        raise FixtureError("manifest has no existing files")
    return sorted(existing, key=lambda item: item["path"])[fallback_index % len(existing)]


def apply_mutation(root: Path, manifest: dict, name: str, out_manifest_path: Path) -> dict:
    files = manifest_entries(manifest)
    profile = manifest.get("profile", "unknown")
    active = {path: dict(entry) for path, entry in files.items() if not entry.get("deleted")}
    tombstones = [dict(entry) for entry in files.values() if entry.get("deleted")]
    changed_paths: set[str] = set()

    def replace(entry: dict) -> None:
        active[entry["path"]] = entry
        changed_paths.add(entry["path"])

    if name == "delta":
        replace(
            rewrite_small_delta(
                root,
                first_existing(active, ["data/model-000.bin"]),
                "delta:model-000",
            )
        )
        replace(
            append_small_delta(
                root,
                first_existing(active, ["data/model-001.bin"]),
                "delta:model-001",
            )
        )
        rename_source = first_existing(active, ["data/model-002.bin"], 2)
        active.pop(rename_source["path"], None)
        changed_paths.add(rename_source["path"])
        replace(rename_entry(root, rename_source, "data/renamed-model-002.bin", "delta"))
        delete_source = first_existing(active, ["data/model-003.bin"], 3)
        active.pop(delete_source["path"], None)
        changed_paths.add(delete_source["path"])
        tombstones.append(delete_entry(root, delete_source, "delta"))
        replace(add_new_entry(root, "data/new-delta-file.bin", 128 * MIB, 0xD317A, "delta"))
    elif name in {"team_alice", "stale_alice"}:
        replace(
            rewrite_small_delta(
                root,
                first_existing(active, ["data/model-000.bin", "data/renamed-model-002.bin"]),
                name,
            )
        )
    elif name in {"team_bob", "stale_bob"}:
        replace(
            append_small_delta(
                root,
                first_existing(active, ["data/model-001.bin", "data/new-delta-file.bin"], 1),
                name,
            )
        )
    elif name == "conflict_a":
        replace(
            rewrite_small_delta(
                root,
                first_existing(active, ["data/model-000.bin", "data/renamed-model-002.bin"]),
                "conflict-a",
            )
        )
    elif name == "conflict_b":
        replace(
            rewrite_small_delta(
                root,
                first_existing(active, ["data/model-000.bin", "data/renamed-model-002.bin"]),
                "conflict-b",
            )
        )
    else:
        raise FixtureError(f"unknown mutation {name!r}")

    updated = build_manifest(
        profile=profile,
        root=root,
        files=[*active.values(), *tombstones],
        mutation=name,
        changed_paths=changed_paths,
    )
    write_manifest(out_manifest_path, updated)
    return updated


def scan_manifest(root: Path, profile: str, out_manifest_path: Path) -> dict:
    files = []
    for path in sorted(root.rglob("*.bin")):
        if ".git" in path.parts or ".crab" in path.parts:
            continue
        if path.name.startswith("._"):
            continue
        size, digest = hash_file(path)
        files.append(
            {
                "path": relpath(path, root),
                "version": 0,
                "size": size,
                "sha256": digest,
                "seed": None,
                "operations": [{"op": "scan"}],
            }
        )
    manifest = build_manifest(profile=profile, root=root, files=files, mutation="scan")
    write_manifest(out_manifest_path, manifest)
    return manifest


def verify_manifest(root: Path, manifest: dict) -> list[dict]:
    results: list[dict] = []
    for entry in manifest.get("files", []):
        relative = entry["path"]
        path = safe_join(root, relative)
        if entry.get("deleted"):
            exists = path.exists()
            results.append(
                {
                    "path": relative,
                    "ok": not exists,
                    "actual_exists": exists,
                    "reason": "deleted" if not exists else "deleted-path-still-exists",
                }
            )
            continue
        if not path.exists():
            results.append(
                {
                    "path": relative,
                    "ok": False,
                    "reason": "missing",
                    "expected_size": entry["size"],
                    "expected_sha256": entry["sha256"],
                }
            )
            continue
        size, digest = hash_file(path)
        ok = size == entry["size"] and digest == entry["sha256"]
        results.append(
            {
                "path": relative,
                "ok": ok,
                "actual_size": size,
                "actual_sha256": digest,
                "expected_size": entry["size"],
                "expected_sha256": entry["sha256"],
                "reason": "ok" if ok else "hash-or-size-mismatch",
            }
        )
    return results


def assert_manifest(root: Path, manifest: dict) -> None:
    failures = [result for result in verify_manifest(root, manifest) if not result["ok"]]
    if failures:
        sample = json.dumps(failures[:5], indent=2, sort_keys=True)
        raise FixtureError(f"manifest verification failed under {root}:\n{sample}")


def copy_manifest(src: Path, dst: Path) -> dict:
    manifest = load_manifest(src)
    write_manifest(dst, manifest)
    return manifest


def remove_fixture_tree(root: Path) -> None:
    if root.exists():
        shutil.rmtree(root)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    create = sub.add_parser("create", help="create a profile under a root")
    create.add_argument("--profile", choices=sorted(PROFILES), required=True)
    create.add_argument("--root", type=Path, required=True)
    create.add_argument("--manifest", type=Path, required=True)

    mutate = sub.add_parser("mutate", help="apply a named mutation")
    mutate.add_argument("--root", type=Path, required=True)
    mutate.add_argument("--manifest", type=Path, required=True)
    mutate.add_argument("--out-manifest", type=Path, required=True)
    mutate.add_argument("--name", required=True)

    verify = sub.add_parser("verify", help="verify files against a manifest")
    verify.add_argument("--root", type=Path, required=True)
    verify.add_argument("--manifest", type=Path, required=True)

    scan = sub.add_parser("scan", help="scan existing *.bin files into a manifest")
    scan.add_argument("--root", type=Path, required=True)
    scan.add_argument("--profile", default="scan")
    scan.add_argument("--manifest", type=Path, required=True)

    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.cmd == "create":
            create_profile(args.root, args.profile, args.manifest)
        elif args.cmd == "mutate":
            manifest = load_manifest(args.manifest)
            apply_mutation(args.root, manifest, args.name, args.out_manifest)
        elif args.cmd == "verify":
            assert_manifest(args.root, load_manifest(args.manifest))
        elif args.cmd == "scan":
            scan_manifest(args.root, args.profile, args.manifest)
        else:
            raise FixtureError(f"unhandled command {args.cmd}")
        return 0
    except FixtureError as exc:
        print(f"error: {exc}", file=os.sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
