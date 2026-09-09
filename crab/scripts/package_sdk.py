#!/usr/bin/env python3
"""Package the SDK closure and compile external consumers from the tarballs."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import time
import tomllib

ROOT = Path(__file__).resolve().parents[2]
VERSION = "0.1.0"
CLOSURE = [
    "crab-types", "crab-xet", "crab-git", "crab-auth", "crab-storage",
    "crab-auth-store", "crab-coordination", "crab-cache", "crab-cache-store",
    "crab-diff", "crab-lfs", "crab-metadata", "crab-remote-git", "crab-read",
    "crab-write", "crab-remote", "crab-staging", "crab-sdk",
]
PROFILES = ["minimal", "remote-content", "local", "managed"]


def run(command: list[str], *, cwd: Path, env: dict[str, str]) -> str:
    completed = subprocess.run(command, cwd=cwd, env=env, check=False, text=True,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if completed.returncode != 0:
        print(completed.stdout, file=sys.stderr, end="")
        completed.check_returncode()
    return completed.stdout


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def safe_extract(archive: Path, destination: Path) -> Path:
    with tarfile.open(archive, "r:gz") as package:
        members = package.getmembers()
        roots = {Path(member.name).parts[0] for member in members if Path(member.name).parts}
        if len(roots) != 1 or any(member.islnk() or member.issym() for member in members):
            raise RuntimeError(f"unsafe package layout: {archive}")
        destination_root = destination.resolve()
        for member in members:
            target = (destination / member.name).resolve()
            if destination_root not in target.parents and target != destination_root:
                raise RuntimeError(f"package escapes extraction root: {member.name}")
        package.extractall(destination, filter="data")
    return destination / roots.pop()


def dependency_has_path(value: object) -> bool:
    return isinstance(value, dict) and "path" in value


def manifest_has_path_dependency(path: Path) -> bool:
    manifest = tomllib.loads(path.read_text())
    sections = [manifest]
    sections.extend(manifest.get("target", {}).values())
    return any(
        dependency_has_path(value)
        for section in sections
        for kind in ("dependencies", "dev-dependencies", "build-dependencies")
        for value in section.get(kind, {}).values()
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True)
    parser.add_argument("--toolchain", default="1.91.1")
    parser.add_argument("--allow-dirty", action="store_true")
    args = parser.parse_args()
    output = args.output.resolve()
    target = args.target_dir.resolve()
    if ROOT.resolve() in output.parents or output == ROOT.resolve():
        raise SystemExit("package qualification output must be outside the source workspace")
    output.mkdir(parents=True, exist_ok=False)
    target.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(target)
    env["CARGO_INCREMENTAL"] = "0"
    env["RUSTC_WRAPPER"] = ""

    command = ["cargo", f"+{args.toolchain}", "package", "--locked", "--no-verify"]
    if args.allow_dirty:
        command.append("--allow-dirty")
    for package in CLOSURE:
        command.extend(["-p", package])
    started = time.monotonic()
    package_log = run(command, cwd=ROOT, env=env)

    package_directory = target / "package"
    vendor = output / "vendor"
    vendor.mkdir()
    packages = []
    patch = ["[patch.crates-io]"]
    for name in CLOSURE:
        archive = package_directory / f"{name}-{VERSION}.crate"
        if not archive.is_file():
            raise RuntimeError(f"Cargo did not create {archive}")
        extracted = safe_extract(archive, vendor)
        if manifest_has_path_dependency(extracted / "Cargo.toml"):
            raise RuntimeError(f"normalized package retains a path dependency: {name}")
        patch.append(f'{name} = {{ path = {json.dumps(str(extracted))} }}')
        packages.append({"name": name, "archive": archive.name,
                         "bytes": archive.stat().st_size, "sha256": sha256(archive)})

    consumer = output / "consumer"
    shutil.copytree(ROOT / "crates/crab-sdk/tests/package-consumer", consumer)
    config = consumer / ".cargo"
    config.mkdir()
    (config / "config.toml").write_text("\n".join(patch) + "\n")
    profile_results = []
    for profile in PROFILES:
        before = time.monotonic()
        log = run(["cargo", f"+{args.toolchain}", "check", "--manifest-path",
                   str(consumer / profile / "Cargo.toml")], cwd=consumer, env=env)
        log_path = output / f"consumer-{profile}.log"
        log_path.write_text(log)
        profile_results.append({"profile": profile,
                                "elapsed_seconds": round(time.monotonic() - before, 3),
                                "terminal_state": "passed", "log": log_path.name})

    report = {
        "schema": "crab.sdk-package-qualification", "version": 1,
        "source_sha": run(["git", "rev-parse", "HEAD"], cwd=ROOT, env=env).strip(),
        "toolchain": run(["rustc", f"+{args.toolchain}", "--version"], cwd=ROOT, env=env).strip(),
        "cargo": run(["cargo", f"+{args.toolchain}", "--version"], cwd=ROOT, env=env).strip(),
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "packages": packages, "profiles": profile_results,
        "terminal_state": "passed",
    }
    (output / "package.log").write_text(package_log)
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(output / "report.json")


if __name__ == "__main__":
    main()
