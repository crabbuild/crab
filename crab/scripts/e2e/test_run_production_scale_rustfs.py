#!/usr/bin/env python3
"""Validate the production-scale RustFS workload contract without running it."""

from __future__ import annotations

import argparse
import sys
import tempfile
from collections import Counter
from pathlib import Path


SCRIPT_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIR))

import run_production_scale_rustfs as workflow


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def source_initialization_creates_gitignore_for_metadata_stage() -> None:
    with tempfile.TemporaryDirectory() as temporary_directory:
        runner = workflow.ProductionScaleRunner(
            argparse.Namespace(
                run_id="metadata-contract",
                root=Path(temporary_directory),
                bucket="crab",
                endpoint_url="http://127.0.0.1:9000",
                jobs=1,
                upload_concurrency=1,
                skip_install=True,
            )
        )
        runner.run_git = lambda *_args, **_kwargs: None
        runner.run_crab = lambda *_args, **_kwargs: None
        runner.init_source()

        require(
            (runner.source / ".gitignore").read_text(encoding="utf-8") == "._*\n**/._*\n.DS_Store\n",
            "source initialization must create .gitignore before metadata staging",
        )


def main() -> int:
    plan = workflow.build_workload_plan()
    files = plan.files
    sizes = [file.size for file in files]
    extensions = Counter(Path(file.path).suffix for file in files)

    require(plan.logical_bytes == 200 * workflow.GIB, "logical size must be 200 GiB")
    require(len(files) == 551, "workload must contain 551 files")
    require(min(sizes) == 50 * workflow.MIB, "smallest file must be 50 MiB")
    require(max(sizes) == 5 * workflow.GIB, "largest file must be 5 GiB")
    require(len({file.path for file in files}) == len(files), "paths must be unique")
    require(
        extensions == {
            ".safetensors": 10,
            ".parquet": 40,
            ".arrow": 100,
            ".bin": 200,
            ".jsonl": 200,
            ".ckpt": 1,
        },
        "file-type mix changed unexpectedly",
    )
    require(
        plan.version_target.path == "models/foundation-model-000.safetensors",
        "version target must be the 5 GiB foundation model",
    )
    require(plan.version_target.size == 5 * workflow.GIB, "version target must be 5 GiB")
    require(plan.version_count == 12, "version count must stay within the requested 10-20 range")
    require(plan.version_patch_bytes == 16 * workflow.MIB, "version patch must be 16 MiB")
    source_initialization_creates_gitignore_for_metadata_stage()
    print("PASS production-scale workload contract")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
