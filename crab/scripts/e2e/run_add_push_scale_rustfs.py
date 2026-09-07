#!/usr/bin/env python3
"""Qualify add/push with 100-GiB content and cold cross-repo chunk reuse.

Uses a fresh bucket and disposable run directory. Ten distinct non-zero bases
are copied with filesystem copy-on-write, then edited independently. The default
workload is 100 GiB logical with 20 GiB of unique initial entropy, not sparse zeros.
Reuse the ordinary smoke runner's command logs, credentials, and S3 contracts.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from run_add_commit_push_rustfs_smoke import AddCommitPushSmoke, sha256_file

MIB = 1024 * 1024


def verify_parallel_proofs(runner: AddCommitPushSmoke, paths: list[Path]) -> None:
    source_cache = runner.env["CRAB_CACHE_DIR"]
    inventories = []
    for jobs in (1, 4):
        runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / f"proof-cache-{jobs}")
        repo, _, _ = runner.prepare_repo(f"proof-workers-{jobs}")
        for index, path in enumerate(paths[:4]):
            with path.open("rb") as source:
                (repo / f"probe-{index}.bin").write_bytes(source.read(16 * MIB))
        runner.run_crab(repo, ["add", "--jobs", str(jobs), "--jsonl", "*.bin"],
                        name=f"proof classification with {jobs} workers")
        inventories.append(runner.staging_payload_inventory(repo))
    runner.env["CRAB_CACHE_DIR"] = source_cache
    runner.check("parallel-proof-classification-preserves-serial-coverage",
                 inventories[0]["recipe_remote_chunks"] > 0 and inventories[0] == inventories[1],
                 {"serial": inventories[0], "parallel": inventories[1]})


def run(args: argparse.Namespace) -> None:
    runner = AddCommitPushSmoke(args)
    if runner.run_root.exists():
        raise RuntimeError("use a fresh run directory")
    try:
        verify(args, runner)
    except Exception as error:
        runner.report.status = "failed"
        runner.report.artifacts["failure"] = str(error)
        runner.write_report()
        raise


def verify(args: argparse.Namespace, runner: AddCommitPushSmoke) -> None:
    status, _, _ = runner.signed_s3_request("HEAD", "")
    runner.check("fresh-bucket", status == 404, {"head_status": status})
    runner.preflight()
    required = args.files * args.file_mib * MIB * 2 + 20 * 1024**3
    runner.check("disk-capacity", shutil.disk_usage(args.root).free >= required,
                 {"required_bytes": required})
    repo, remote, _ = runner.prepare_repo("scale")
    outside = runner.run_root / "symlink-target"
    outside.mkdir()
    (outside / "model.bin").write_bytes(b"external bytes must not enter staging")
    (repo / "linked").symlink_to(outside, target_is_directory=True)
    runner.run_crab(repo, ["add", "linked/model.bin"], name="symlink ancestor add", check=False)
    index = repo / ".crab" / "staging" / "index.db"
    inventory = runner.staging_payload_inventory(repo) if index.exists() else {}
    runner.check("symlink-ancestor-created-no-payload", not any(inventory.values()), inventory)
    size = args.file_mib * MIB
    models = repo / "models"
    models.mkdir()
    paths = [models / f"model-{index:03}.bin" for index in range(args.files)]
    families = min(10, len(paths))
    available_before = shutil.disk_usage(args.root).free
    for index, path in enumerate(paths):
        if index < families:
            with path.open("wb") as stream:
                for block in range(args.file_mib):
                    stream.write(hashlib.shake_256(f"family:{index}:{block}".encode()).digest(MIB))
        else:
            clone_flag = "-c" if sys.platform == "darwin" else "--reflink=always"
            runner.run_cmd(f"copy-on-write fixture {index}",
                           ["cp", clone_flag, str(paths[index % families]), str(path)], repo)
    code = repo / "src"
    code.mkdir()
    for index in range(args.code_files):
        (code / f"module_{index:04}.rs").write_text(f"pub const VALUE: u64 = {index};\n")
    runner.check("workload-shape", True, {
        "large_files": len(paths), "logical_bytes": sum(p.stat().st_size for p in paths),
        "distinct_basis_bytes": families * size,
        "allocated_file_bytes_including_shared_extents": sum(p.stat().st_blocks * 512 for p in paths),
        "observed_free_space_delta": available_before - shutil.disk_usage(args.root).free,
        "small_code_files": args.code_files, "versions": args.versions,
    })
    for version in range(args.versions):
        if version:
            for index, path in enumerate(paths):
                with path.open("r+b") as stream:
                    stream.seek(size // 2 + version * MIB)
                    stream.write(hashlib.shake_256(f"edit:{version}:{index}".encode()).digest(MIB))
            for index in range(args.code_files):
                with (code / f"module_{index:04}.rs").open("a") as stream:
                    stream.write(f"pub const VERSION_{version}: u64 = {version};\n")
        if version == 1:
            selected = str(paths[0].relative_to(repo))
            runner.run_crab(repo, ["add", "--jsonl", "--skip-git-add", selected], name="deferred preparation")
            runner.run_git(repo, ["add", selected], name="deferred Git publication")
        if version == 2 and len(paths) > 1:
            with ThreadPoolExecutor(max_workers=2) as pool:
                pending = [pool.submit(runner.run_crab, repo,
                           ["add", "--jsonl", str(path.relative_to(repo))], name=f"concurrent add {path.name}")
                           for path in paths[:2]]
                for result in pending:
                    result.result()
        runner.run_crab(repo, ["add", "--jsonl", "models/"], name=f"v{version} add")
        runner.run_git(repo, ["add", "src"])
        runner.run_git(repo, ["commit", "-m", f"version {version}"])
        runner.run_crab(repo, ["push", "--jsonl", "origin", "HEAD:refs/heads/main"],
                        name=f"v{version} push", timeout=args.push_timeout)
        if version == 0:
            verify_parallel_proofs(runner, paths)

    expected = {str(path.relative_to(repo)): sha256_file(path)
                for path in [*paths, *sorted(code.iterdir())]}
    (runner.artifacts / "expected-sha256.json").write_text(json.dumps(expected, indent=2))

    # No source cache or prepared proof can mask the consumer's remote lookup.
    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "consumer-cache")
    consumer, consumer_remote, _ = runner.prepare_repo("cold-consumer")
    consumer_file = consumer / "model.bin"
    with paths[0].open("rb") as source, consumer_file.open("wb") as output:
        for _ in range(512):
            output.write(source.read(MIB))
        output.write(hashlib.shake_256(b"consumer-only-tail").digest(MIB))
    consumer_digest = sha256_file(consumer_file)
    runner.run_git(consumer, ["add", "model.bin"])
    inventory = runner.staging_payload_inventory(consumer)
    runner.check("cold-consumer-exceeds-one-candidate-page", inventory["chunk_payloads"] > 4096, inventory)
    runner.run_git(consumer, ["commit", "-m", "reuse chunks with a distinct file hash"])
    before = runner.list_keys(".crab/xorbs/")
    runner.run_crab(consumer, ["push", "--log-level", "debug", "origin", "HEAD:refs/heads/main"],
                    name="cold cross-repository push", timeout=args.push_timeout)
    added = runner.list_keys(".crab/xorbs/") - before
    runner.check("cold-consumer-reuses-shared-chunks", len(added) <= 1, {"new_xorbs": len(added)})
    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "consumer-clone-cache")
    consumer_clone = runner.run_root / "consumer-clone"
    runner.run_cmd("consumer clone", [runner.crab_bin, "clone", consumer_remote, str(consumer_clone)], runner.run_root)
    runner.run_crab(consumer_clone, ["hydrate", "--all"])
    runner.check("consumer-byte-identity", sha256_file(consumer_clone / "model.bin") == consumer_digest)

    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "cold-clone-cache")
    clone = runner.run_root / "clone"
    runner.run_cmd("scale clone", [runner.crab_bin, "clone", remote, str(clone)], runner.run_root)
    for cycle in ("cold", "rehydrated"):
        runner.run_crab(clone, ["hydrate", "--all"], name=f"{cycle} hydrate")
        for relative, digest in expected.items():
            runner.check(f"{cycle}-bytes-{relative}", sha256_file(clone / relative) == digest)
        runner.run_crab(clone, ["dehydrate", "--all"], name=f"{cycle} dehydrate")
        for path in paths:
            pointer = clone / path.relative_to(repo)
            runner.check(f"{cycle}-pointer-{path.name}",
                         pointer.stat().st_size < 1024 and pointer.read_text().startswith("version https://crab.build/spec/v1"))
    runner.run_git(clone, ["fsck", "--full", "--strict"])
    runner.check_credential_disclosure()
    runner.report.status = "passed"
    runner.write_report()
    if args.cleanup:
        # All targets were created by this invocation; retain reports and logs.
        for path in (repo.parent, consumer.parent, clone, consumer_clone,
                     runner.cache_dir, runner.run_root / "consumer-cache",
                     runner.run_root / "consumer-clone-cache", runner.run_root / "cold-clone-cache", outside,
                     runner.run_root / "proof-cache-1", runner.run_root / "proof-cache-4",
                     runner.run_root / "proof-workers-1", runner.run_root / "proof-workers-4"):
            if path.exists():
                shutil.rmtree(path)
        runner.run_cmd("clean isolated bucket", ["aws", "--endpoint-url", args.endpoint_url,
                       "s3", "rb", f"s3://{args.bucket}", "--force"], runner.run_root)
        runner.check("isolated-data-cleaned", True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--crab-bin", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--endpoint-url", default="http://127.0.0.1:9000")
    parser.add_argument("--files", type=int, default=50)
    parser.add_argument("--file-mib", type=int, default=2048)
    parser.add_argument("--code-files", type=int, default=500)
    parser.add_argument("--versions", type=int, default=3)
    parser.add_argument("--cleanup", action="store_true")
    args = parser.parse_args()
    if args.files < 1 or args.file_mib < 1024 or not 1 <= args.versions <= 10 or args.code_files < 1:
        parser.error("require positive file counts, >=1024 MiB/file, and 1–10 versions")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.run_id):
        parser.error("run-id must be a single safe directory name")
    args.access_key = "crab"
    args.secret_key = "crab"
    args.session_token = ""
    args.region = "us-east-1"
    args.timeout = 3600
    args.push_timeout = 3600
    args.source = None
    run(args)


if __name__ == "__main__":
    main()
