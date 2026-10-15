#!/usr/bin/env python3
"""Qualify repeated 10-GiB edits through both Git clean and native Crab push.

Two copy-on-write files start with identical, non-sparse deterministic content.
Choose entropy, a repeated one-MiB block, or zero-filled bytes.
Edits diverge at the beginning, middle and end. Each committed version must
hydrate byte-identically from a fresh cache after the source repository is removed.
Only this invocation's disposable data is released; logs and receipts remain.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import sys
from pathlib import Path

from run_add_commit_push_rustfs_smoke import AddCommitPushSmoke, REPO_ROOT, sha256_file
from run_add_push_scale_rustfs import object_inventory, release_verified_run_child
from run_concurrent_push_smoke import RequestCountingProxy

MIB = 1024**2
GIB = 1024**3
CACHE_BYTES = 2 * GIB


class MeasuredRunner(AddCommitPushSmoke):
    def run_cmd(self, name, args, cwd, **kwargs):
        timed = args[0] in (self.crab_bin, "git")
        if timed:
            args = ["/usr/bin/time", "-l" if sys.platform == "darwin" else "-v", *args]
        result = super().run_cmd(name, args, cwd, **kwargs)
        if timed:
            stderr = Path(result.stderr_log).read_text()
            pattern = (r"(\d+)\s+maximum resident set size" if sys.platform == "darwin"
                       else r"Maximum resident set size \(kbytes\):\s*(\d+)")
            match = re.search(pattern, stderr)
            if match:
                self.report.commands[-1]["max_rss_bytes"] = int(match[1]) * (1 if sys.platform == "darwin" else 1024)
            if sys.platform == "darwin":
                cpu = re.search(r"([\d.]+) user\s+([\d.]+) sys", stderr)
                if cpu:
                    self.report.commands[-1]["cpu_seconds"] = float(cpu[1]) + float(cpu[2])
            else:
                user = re.search(r"User time \(seconds\):\s*([\d.]+)", stderr)
                system = re.search(r"System time \(seconds\):\s*([\d.]+)", stderr)
                if user and system:
                    self.report.commands[-1]["cpu_seconds"] = float(user[1]) + float(system[1])
            self.write_report()
        return result


def cap_cache(repo: Path) -> None:
    config = repo / ".crab" / "local.toml"
    with config.open("a") as stream:
        stream.write(f"\n[cache]\nmax_bytes = {CACHE_BYTES}\n")


def qualify(runner: AddCommitPushSmoke, proxy: RequestCountingProxy) -> None:
    args = runner.args
    size = args.file_mib * MIB
    runner.check("fresh-bucket", runner.signed_s3_request("HEAD", "")[0] == 404)
    runner.preflight()
    patch = runner.run_git(REPO_ROOT, ["diff", "HEAD", "--binary"], name="qualified source patch")
    runner.report.artifacts["source_patch"] = patch.stdout_log
    runner.report.artifacts["source_patch_sha256"] = sha256_file(Path(patch.stdout_log))
    runner.report.artifacts["harness_sha256"] = sha256_file(Path(__file__))
    # Source, staging (including a new version), origin, active cache and reserve.
    required = 4 * size + CACHE_BYTES + 10 * GIB
    available = shutil.disk_usage(args.root).free
    runner.check("disk-capacity", available >= required,
                 {"required_bytes": required, "available_bytes": available})
    scratch = runner.run_root / "tmp"
    scratch.mkdir()
    runner.env["TMPDIR"] = str(scratch)
    repo, remote, prefix = runner.prepare_repo("source")
    cap_cache(repo)
    paths = [repo / "model-a.bin", repo / "model-b.bin"]
    repeated_block = None
    if args.pattern == "zero":
        repeated_block = bytes(MIB)
    elif args.pattern == "repeated":
        repeated_block = hashlib.shake_256(b"large-edits:repeated").digest(MIB)
    with paths[0].open("wb") as output:
        for block in range(args.file_mib):
            output.write(repeated_block if repeated_block is not None else
                         hashlib.shake_256(f"large-edits:{block}".encode()).digest(MIB))
    clone_flag = "-c" if sys.platform == "darwin" else "--reflink=always"
    runner.run_cmd("copy-on-write second large file",
                   ["cp", clone_flag, str(paths[0]), str(paths[1])], repo)
    code = repo / "src"
    code.mkdir()
    for index in range(args.code_files):
        (code / f"module-{index:05}.rs").write_text(f"pub const VALUE: u64 = {index};\n")
    runner.check("workload", all(path.stat().st_size == size for path in paths),
                 {"file_bytes": size, "files": 2, "initial_unique_bytes": size,
                  "versions": 4, "code_files": args.code_files, "pattern": args.pattern})
    history = []
    phases = []

    def record() -> None:
        receipt = runner.artifacts / "large-file-edits.json"
        receipt.write_text(json.dumps({"history": history, "phases": phases, "pattern": args.pattern,
                                      "transport": proxy.snapshot()}, indent=2) + "\n")
        runner.report.artifacts["large_file_edits"] = str(receipt)
        runner.write_report()

    for version in range(4):
        if version:
            offset = (0, size // 2, size - MIB)[version - 1]
            for index, path in enumerate(paths):
                with path.open("r+b") as stream:
                    stream.seek(offset)
                    stream.write(hashlib.shake_256(f"edit:{version}:{index}".encode()).digest(MIB))
            (code / "module-00000.rs").write_text(f"pub const VERSION: u64 = {version};\n")
        before = proxy.snapshot()
        if version % 2:
            add = runner.run_git(repo, ["add", "--", *[p.name for p in paths]],
                                 name=f"v{version} Git clean-filter add")
        else:
            add = runner.run_crab(repo, ["add", "--jsonl", "*.bin"], name=f"v{version} Crab add")
        staging = repo / ".crab" / "staging"
        local_bytes = {
            name: sum(path.stat().st_size for path in (staging / name).rglob("*") if path.is_file())
            for name in ("segments", "push-plans")
        }
        phases.append({"version": version, "phase": "add", "duration_ms": add.duration_ms,
                       "staging_directory_bytes": local_bytes,
                       "transport": RequestCountingProxy.delta(before, proxy.snapshot())})
        for path in paths:
            runner.assert_index_pointer(repo, path.name, size)
        runner.run_git(repo, ["add", "src"])
        runner.run_git(repo, ["commit", "-m", f"large-file version {version}"])
        before_inventory = object_inventory(runner, ".crab/xorbs/")
        before = proxy.snapshot()
        concurrency = 1 if version == 1 else 4
        push = runner.run_crab(repo, ["push", "--jsonl", "--upload-concurrency", str(concurrency), "origin", "HEAD:refs/heads/main"],
                               name=f"v{version} Crab push")
        transport = RequestCountingProxy.delta(before, proxy.snapshot())
        after_inventory = object_inventory(runner, ".crab/xorbs/")
        added_bytes = after_inventory["bytes"] - before_inventory["bytes"]
        phases.append({"version": version, "phase": "push", "duration_ms": push.duration_ms,
                       "new_xorb_bytes": added_bytes, "xorbs": after_inventory, "transport": transport,
                       "upload_concurrency": concurrency})
        expected = {path.name: sha256_file(path) for path in paths}
        history.append({"version": version, "commit": runner.rev_parse(repo, "HEAD"), "files": expected})
        record()
        runner.check(f"v{version}-published-tip",
                     runner.ls_remote(remote, name=f"v{version} published refs").get("refs/heads/main") == history[-1]["commit"])
        if version:
            runner.check(f"v{version}-incremental-upload", 0 < added_bytes < size // 20,
                         {"new_xorb_bytes": added_bytes, "edited_bytes": 2 * MIB})
        else:
            runner.check("initial-identical-files-share-xorbs", 0 < added_bytes < size * 102 // 100,
                         {"new_xorb_bytes": added_bytes, "unique_source_bytes": size})
        runner.check(f"v{version}-transport-clean", not transport["proxy_errors"], transport)
    runner.check("v2-root", bool(runner.list_keys(f"{prefix}/v2/root")))
    runner.run_git(repo, ["fsck", "--full", "--strict"])
    # Remove all source authority before proving any historical reconstruction.
    release_verified_run_child(runner, repo.parent)
    release_verified_run_child(runner, runner.cache_dir)
    clone = runner.run_root / "clone"
    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "clone-cache")
    runner.run_cmd("independent clone", [runner.crab_bin, "clone", remote, str(clone)], runner.run_root)
    cap_cache(clone)
    for snapshot in history:
        version = snapshot["version"]
        cache = runner.run_root / f"history-cache-{version}"
        runner.env["CRAB_CACHE_DIR"] = str(cache)
        runner.run_git(clone, ["checkout", "--detach", snapshot["commit"]], name=f"v{version} checkout")
        before = proxy.snapshot()
        hydrate = runner.run_crab(clone, ["hydrate", "--all"], name=f"v{version} cold hydrate")
        phases.append({"version": version, "phase": "cold-hydrate", "duration_ms": hydrate.duration_ms,
                       "transport": RequestCountingProxy.delta(before, proxy.snapshot())})
        for name, expected in snapshot["files"].items():
            runner.check(f"v{version}-bytes-{name}", sha256_file(clone / name) == expected)
        runner.run_crab(clone, ["dehydrate", "--all"], name=f"v{version} dehydrate")
        release_verified_run_child(runner, cache)
        record()
    runner.run_git(clone, ["fsck", "--full", "--strict"])
    result = runner.run_crab(clone, ["fsck", "--json"], name="remote integrity")
    runner.report.artifacts["remote_fsck"] = result.stdout_log
    integrity = json.loads(runner.read_stdout(result))["data"]
    runner.check("remote-integrity", integrity["passed"] and integrity["errors"] == 0
                 and integrity["repair_failures"] == 0, integrity)
    runner.check("transport-clean", not proxy.snapshot()["proxy_errors"])
    runner.check("binary-unchanged", sha256_file(Path(runner.crab_bin)) == runner.report.artifacts["crab_binary_sha256"])
    runner.check_credential_disclosure()
    runner.report.status = "passed"
    record()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--crab-bin", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--endpoint-url", required=True)
    parser.add_argument("--file-mib", type=int, default=10240)
    parser.add_argument("--code-files", type=int, default=5000)
    parser.add_argument("--pattern", choices=("entropy", "repeated", "zero"), default="entropy")
    args = parser.parse_args()
    if args.file_mib < 16 or args.code_files < 1 or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.run_id):
        parser.error("require >=16 MiB/file, positive code files and a safe run id")
    args.root.mkdir(parents=True, exist_ok=True)
    (args.root / args.run_id).mkdir()
    args.access_key, args.secret_key, args.session_token = "crab", "crab", ""
    args.region, args.timeout, args.push_timeout, args.source = "us-east-1", 7200, 7200, None
    runner = MeasuredRunner(args)
    proxy = RequestCountingProxy(args.endpoint_url, args.bucket)
    try:
        proxy.start()
        runner.env["AWS_ENDPOINT_URL"] = proxy.url
        runner.env["AWS_ENDPOINT_URL_S3"] = proxy.url
        qualify(runner, proxy)
    except Exception as error:
        runner.report.status = "failed"
        runner.report.artifacts["failure"] = str(error)
        raise
    finally:
        runner.write_report()
        proxy.close()


if __name__ == "__main__":
    main()
