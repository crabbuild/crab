#!/usr/bin/env python3
"""Qualify native recipe verification, tail reuse and history on real RustFS."""

import argparse
import hashlib
import json
import os
import re
import shutil
from pathlib import Path

from run_add_commit_push_rustfs_smoke import REPO_ROOT, sha256_file
from run_add_push_scale_rustfs import release_verified_run_child
from run_large_file_edits_rustfs import GIB, MIB, MeasuredRunner, cap_cache


def qualify(runner):
    runner.check("fresh-bucket", runner.signed_s3_request("HEAD", "")[0] == 404)
    runner.preflight()
    patch = runner.run_git(REPO_ROOT, ["diff", "HEAD", "--binary"], name="qualified source patch")
    runner.report.artifacts["source_patch"] = patch.stdout_log
    runner.report.artifacts["source_patch_sha256"] = sha256_file(Path(patch.stdout_log))
    runner.report.artifacts["harness_sha256"] = sha256_file(Path(__file__))
    size = runner.args.file_mib * MIB
    runner.check("disk-capacity", shutil.disk_usage(runner.args.root).free >= 3 * size + 10 * GIB)
    scratch = runner.run_root / "tmp"
    scratch.mkdir()
    runner.env["TMPDIR"] = str(scratch)
    repo, remote, _ = runner.prepare_repo("source")
    cap_cache(repo)
    path = repo / "model.bin"
    with path.open("wb") as stream:
        for block in range(runner.args.file_mib):
            stream.write(hashlib.shake_256(f"native-recipe:{block}".encode()).digest(MIB))
    runner.check("non-sparse-input", path.stat().st_size == size and path.stat().st_blocks * 512 >= size)
    runner.run_crab(repo, ["add", "--jsonl", "model.bin"], name="initial native add")
    runner.assert_index_pointer(repo, "model.bin", size)

    def unchanged(name):
        stat = path.stat()
        os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns + 2_000_000_000))
        runner.run_crab(repo, ["add", "--jsonl", "model.bin"], name=name)
        command = runner.report.commands[-1]
        phases = command.get("ingestion_phases", {})
        runner.check(name + "-full-verification-without-materialization",
                     phases.get("input_bytes") == size
                     and phases.get("chunking_ms") == 0
                     and phases.get("authority_index_ms") == 0
                     and phases.get("compression_ms") == 0
                     and command.get("prepared_payload_file_bytes_written") == 0,
                     phases)
        runner.assert_index_pointer(repo, "model.bin", size)

    unchanged("unchanged native after native")
    runner.run_git(repo, ["add", "--renormalize", "--", "model.bin"], name="unchanged Git clean")
    unchanged("unchanged native after Git clean")
    runner.run_git(repo, ["commit", "-m", "initial large file"])
    history = [(runner.rev_parse(repo, "HEAD"), sha256_file(path))]
    with path.open("r+b") as stream:
        stream.seek(size - MIB)
        stream.write(hashlib.shake_256(b"native-tail-edit").digest(MIB))
    runner.run_crab(repo, ["add", "--jsonl", "model.bin"], name="native tail edit before push")
    command = runner.report.commands[-1]
    runner.check("tail-edit-bounded-payload-write",
                 0 < command.get("prepared_payload_file_bytes_written", 0) < 4 * MIB,
                 command.get("ingestion_phases", {}))
    unchanged("unchanged native after tail edit")
    runner.run_git(repo, ["commit", "-m", "native tail edit"])
    history.append((runner.rev_parse(repo, "HEAD"), sha256_file(path)))
    runner.run_crab(repo, ["push", "--jsonl", "origin", "HEAD:refs/heads/main"], name="publish both versions")
    runner.run_git(repo, ["fsck", "--full", "--strict"])
    release_verified_run_child(runner, repo.parent)
    release_verified_run_child(runner, runner.cache_dir)
    runner.check("source-and-staging-removed", not repo.exists())
    clone = runner.run_root / "independent-clone"
    runner.run_crab(runner.run_root, ["clone", remote, str(clone)], name="independent clone")
    cap_cache(clone)
    for version, (commit, expected) in enumerate(history):
        cache = runner.run_root / f"history-cache-{version}"
        runner.env["CRAB_CACHE_DIR"] = str(cache)
        runner.run_git(clone, ["checkout", "--detach", commit], name=f"v{version} checkout")
        runner.run_crab(clone, ["hydrate", "--all"], name=f"v{version} cold hydrate")
        runner.check(f"v{version}-byte-identical", sha256_file(clone / "model.bin") == expected)
        runner.run_crab(clone, ["dehydrate", "--all"], name=f"v{version} dehydrate")
        release_verified_run_child(runner, cache)
    runner.run_git(clone, ["fsck", "--full", "--strict"])
    result = runner.run_crab(clone, ["fsck", "--json"], name="remote integrity")
    integrity = json.loads(runner.read_stdout(result))["data"]
    runner.check("remote-integrity", integrity["passed"] and integrity["errors"] == 0, integrity)
    runner.check("binary-unchanged", sha256_file(Path(runner.crab_bin)) == runner.report.artifacts["crab_binary_sha256"])
    runner.check_credential_disclosure()
    runner.report.status = "passed"
    runner.write_report()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--crab-bin", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--endpoint-url", required=True)
    parser.add_argument("--file-mib", type=int, default=10240)
    args = parser.parse_args()
    if args.file_mib < 16 or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.run_id):
        parser.error("require >=16 MiB and a safe run id")
    args.root.mkdir(parents=True, exist_ok=True)
    (args.root / args.run_id).mkdir()
    args.access_key, args.secret_key, args.session_token = "crab", "crab", ""
    args.region, args.timeout, args.push_timeout, args.source = "us-east-1", 7200, 7200, None
    runner = MeasuredRunner(args)
    try:
        qualify(runner)
    except Exception as error:
        runner.report.status = "failed"
        runner.report.artifacts["failure"] = str(error)
        raise
    finally:
        runner.write_report()


if __name__ == "__main__":
    main()
