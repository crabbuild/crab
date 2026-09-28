#!/usr/bin/env python3
"""Run the qualification validator from Crab's locked Cellule revision."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[2]


def cellule_root() -> Path:
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
    dependency = workspace["workspace"]["dependencies"]["cellule-runtime"]
    revision = dependency["rev"]
    source = f"git+{dependency['git']}?rev={revision}#{revision}"
    metadata = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    packages = json.loads(metadata.stdout)["packages"]
    matches = [
        package for package in packages
        if package["name"] == "cellule-runtime" and package["source"] == source
    ]
    if len(matches) != 1:
        raise ValueError("the locked Cellule runtime does not match Cargo.toml")
    manifest = Path(matches[0]["manifest_path"])
    root = manifest.parents[2]
    if not (root / "Cargo.lock").is_file():
        raise ValueError("the locked Cellule checkout has no Cargo.lock")
    return root


def target_dir() -> Path:
    runner_temp = os.environ.get("RUNNER_TEMP")
    if runner_temp:
        return Path(runner_temp) / "cellule-qualification-target"
    target_root = Path.home() / "Workspace" / "crabbuild-target"
    if not target_root.is_dir() or not os.access(target_root, os.W_OK):
        raise ValueError("the mounted Workspace volume is required for a Cellule build")
    checkout = hashlib.sha256(os.fsencode(ROOT)).hexdigest()[:12]
    return target_root / f"cellule-crab-{checkout}"


def main() -> int:
    if len(sys.argv) < 2 or sys.argv[1] not in {"source-root", "build", "run"}:
        print("usage: cellule-qualification.py source-root|build|run [receipt arguments...]", file=sys.stderr)
        return 2
    source = cellule_root()
    if sys.argv[1] == "source-root":
        print(source)
        return 0
    target = target_dir()
    target.mkdir(parents=True, exist_ok=True)
    command = [
        "cargo", sys.argv[1], "--locked", "--quiet", "--manifest-path",
        str(source / "Cargo.toml"), "-p", "cellule-runtime", "--bin",
        "qualification_receipt",
    ]
    if sys.argv[1] == "run":
        command.extend(["--", *sys.argv[2:]])
    elif len(sys.argv) != 2:
        print("build does not accept receipt arguments", file=sys.stderr)
        return 2
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(target)
    return subprocess.run(command, env=environment, check=False).returncode


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (subprocess.CalledProcessError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
