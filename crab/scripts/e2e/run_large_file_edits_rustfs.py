#!/usr/bin/env python3
"""Qualify large mixed repositories through Git clean and native Crab push.

Two copy-on-write files start with identical, non-sparse deterministic content.
Choose entropy, a repeated one-MiB block, or zero-filled bytes.
Choose native-first or Git-first alternating ingestion.
Edits diverge at the beginning, middle and end. Each committed version must
hydrate byte-identically from a fresh cache after the source repository is removed.
Optional distinct files, LFS assets and lifecycle checks support larger runs.
Only this invocation's disposable data is released; logs and receipts remain.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import select
import shutil
import subprocess
import time
import sys
from pathlib import Path
from urllib.parse import urlsplit

from run_add_commit_push_rustfs_smoke import AddCommitPushSmoke, REPO_ROOT, sha256_file
from run_add_push_scale_rustfs import object_inventory, release_verified_run_child
from run_concurrent_push_smoke import RequestCountingProxy

MIB = 1024**2
GIB = 1024**3
CACHE_BYTES = 2 * GIB


def write_payload(path: Path, mib: int, pattern: str, seed: str) -> None:
    repeated = (bytes(MIB) if pattern == "zero" else
                hashlib.shake_256(seed.encode()).digest(MIB) if pattern == "repeated" else None)
    with path.open("wb") as output:
        for block in range(mib):
            output.write(repeated if repeated is not None else
                         hashlib.shake_256(f"{seed}:{block}".encode()).digest(MIB))


def verify_lfs(runner: AddCommitPushSmoke, repo: Path, expected: str, size: int) -> None:
    record = runner.run_git(repo, ["show", ":asset.lfs"], name="inspect LFS index pointer")
    runner.check("LFS-pointer", runner.read_stdout(record).splitlines() == [
        "version https://git-lfs.github.com/spec/v1", f"oid sha256:{expected}", f"size {size}"])


def verify_history(runner, proxy, clone, history, phases, label):
    for snapshot in history:
        version = snapshot["version"]
        cache = runner.run_root / f"{label}-cache-{version}"
        runner.check(f"{label}-v{version}-cold-cache", not cache.exists())
        runner.env["CRAB_CACHE_DIR"] = str(cache)
        runner.run_git(clone, ["checkout", "--detach", snapshot["commit"]],
                       name=f"{label} v{version} checkout")
        before = proxy.snapshot()
        hydrate = runner.run_crab(clone, ["hydrate", "--all"], name=f"{label} v{version} cold hydrate")
        if runner.args.lfs_mib:
            runner.run_crab(clone, ["lfs", "install", "--local", "--skip-smudge", "--skip-repo"])
            runner.run_crab(clone, ["lfs", "fetch", "origin", snapshot["commit"]])
            runner.run_git(clone, ["lfs", "checkout"])
        phases.append({"version": version, "phase": f"{label}-cold-hydrate",
                       "duration_ms": hydrate.duration_ms,
                       "transport": RequestCountingProxy.delta(before, proxy.snapshot())})
        for name, expected in snapshot["files"].items():
            runner.check(f"{label}-v{version}-bytes-{name}", sha256_file(clone / name) == expected)
        actual_code = {name: sha256_file(clone / name) for name in snapshot["code"]}
        runner.check(f"{label}-v{version}-code", actual_code == snapshot["code"],
                     {"files": len(actual_code)})
        runner.run_crab(clone, ["dehydrate", "--all"], name=f"{label} v{version} dehydrate")
        release_verified_run_child(runner, cache)
        # This disposable clone must re-download LFS bytes for the next oracle.
        # Removing its cache only after hashing keeps history verification cold.
        if runner.args.lfs_mib:
            (clone / "asset.lfs").unlink()
            shutil.rmtree(clone / ".git" / "lfs" / "objects")


def verify_retained_reader(runner, proxy, clone, remote, history):
    snapshot = history[0]
    maintenance = runner.run_root / "maintenance-clone"
    runner.run_cmd("independent maintenance clone", [runner.crab_bin, "clone", remote, str(maintenance)], runner.run_root)
    pointer_paths = []
    names = [name for name in snapshot["files"] if name.endswith(".bin")]
    for name in names:
        record = runner.run_git(clone, ["show", snapshot["commit"] + ":" + name])
        path = runner.artifacts / ("retained-" + name + ".pointer")
        path.write_text(runner.read_stdout(record))
        pointer_paths.append(path)
    reader_cache = runner.run_root / "retained-reader-cache"
    output = runner.run_root / "retained-reader-output"
    prefix = urlsplit(remote).path.lstrip("/")
    args = [str(runner.args.retained_reader_bin), proxy.url, runner.args.bucket,
            prefix, str(reader_cache), str(output), *map(str, pointer_paths)]
    with proxy.lock:
        traced_paths = proxy.trace_paths
        proxy.trace_paths = True
    started = time.monotonic()
    reader = subprocess.Popen(args, cwd=clone, env=runner.env, stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    captured_line, stdout, stderr = "", "", ""
    resumed, finished = False, False
    try:
        ready = select.select([reader.stdout], [], [], min(60, runner.args.timeout))[0]
        runner.check("retained-reader-captured", bool(ready))
        captured_line = reader.stdout.readline()
        captured = json.loads(captured_line)
        runner.check("retained-reader-captured-files", captured["event"] == "captured" and captured["files"] == len(names))
        runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "maintenance-cache")
        root_key = prefix + "/v2/root"
        before = runner.signed_s3_request("GET", root_key)
        shards_before = set(runner.list_keys(".crab/shards/"))
        refs_before = runner.ls_remote(remote, name="captured-reader refs before compaction")
        runner.run_crab(maintenance, ["compact", "--repo", prefix, "--bucket", runner.args.bucket],
                        name="compact Xet shard bindings")
        after = runner.signed_s3_request("GET", root_key)
        shards_after = set(runner.list_keys(".crab/shards/"))
        runner.check("retained-reader-crosses-Xet-compaction", bool(shards_after - shards_before),
                     {"new_shards": len(shards_after - shards_before), "source_shards": len(shards_before)})
        runner.check("retained-reader-compaction-preserves-refs",
                     runner.ls_remote(remote, name="captured-reader refs after compaction") == refs_before)
        runner.check("retained-reader-crosses-checkpoint-replacement", before[0] == after[0] == 200 and before[2] != after[2],
                     {"before_root_sha256": hashlib.sha256(before[2]).hexdigest(),
                      "after_root_sha256": hashlib.sha256(after[2]).hexdigest()})
        runner.run_crab(maintenance, ["gc", "--json"], name="repository GC with captured reader")
        runner.check("retained-reader-held-through-maintenance", reader.poll() is None)
        cursor = proxy.snapshot()["path_count"]
        resumed = True
        stdout, stderr = reader.communicate("resume\n", timeout=runner.args.timeout)
        finished = True
        runner.check("retained-reader-reconstructs", reader.returncode == 0)
        paths = proxy.paths_since(cursor)
        runner.check("retained-reader-no-root-recapture", not any(path["key"].endswith("/v2/root") for path in paths))
        runner.check("retained-reader-cold-catalog", any(path["method"] == "GET" and
                     "/v2/catalogs/" in path["key"] and path["status"] in (200, 206) for path in paths))
        trace = runner.artifacts / "retained-reader-requests.json"
        trace.write_text(json.dumps(paths, indent=2) + "\n")
        runner.report.artifacts["retained_reader_requests"] = str(trace)
        events = [captured] + [json.loads(line) for line in stdout.splitlines()]
        runner.check("retained-reader-closed-captured-view", events[-1]["event"] == "closed" and
                     all(event["root_digest"] == captured["root_digest"] for event in events))
    finally:
        with proxy.lock:
            proxy.trace_paths = traced_paths
        if not finished:
            try:
                stdout, stderr = reader.communicate(None if resumed else "abort\n", timeout=30)
            except subprocess.TimeoutExpired:
                reader.kill()
                stdout, stderr = reader.communicate()
        runner.record_command("retained reader hydrate", args, clone, reader.returncode,
                              int((time.monotonic() - started) * 1000), captured_line + stdout, stderr)
    for index, name in enumerate(names):
        runner.check(f"retained-reader-bytes-{name}", sha256_file(output / f"file-{index}.bin") == snapshot["files"][name])
    runner.check("retained-reader-binary-unchanged", sha256_file(runner.args.retained_reader_bin) ==
                 runner.report.artifacts["retained_reader_binary_sha256"])
    release_verified_run_child(runner, reader_cache)
    release_verified_run_child(runner, output)
    release_verified_run_child(runner, maintenance)


def qualify_lifecycle(runner, clone, prefix):
    for name, command in [
        ("checkpoint after captured-reader compaction", ["repack", "--json"]),
        ("repository GC preview", ["gc", "--dry-run", "--json"]),
        ("repository GC preserving reader grace", ["gc", "--json"]),
    ]:
        runner.run_crab(clone, command, name=name)
    listing = runner.run_crab(clone, ["recover", "history", "list", "--json"])
    entries = json.loads(runner.read_stdout(listing))["data"]["entries"]
    runner.check("retained-checkpoints", len(entries) >= 2)
    ordered = sorted(entries, key=lambda entry: entry["generation"])
    for entry in (ordered[0], ordered[-1]):
        selection = [str(entry["generation"]), "--digest", entry["digest"]]
        runner.run_crab(clone, ["recover", "history", "verify", *selection, "--json"])
        root_before = runner.signed_s3_request("GET", prefix + "/v2/root")[2]
        preview = runner.run_crab(clone, ["recover", "history", "restore", *selection, "--json"])
        preview = json.loads(runner.read_stdout(preview))["data"]
        runner.check("restore-preview-read-only", preview["applied"] is False and
                     runner.signed_s3_request("GET", prefix + "/v2/root")[2] == root_before)
        applied = runner.run_crab(clone, ["recover", "history", "restore", *selection, "--apply", "--json"])
        applied = json.loads(runner.read_stdout(applied))["data"]
        runner.check("restore-new-epoch", applied["applied"] is True and
                     applied["restored_generation"] > applied["previous_generation"])
    runner.run_git(clone, ["fsck", "--full", "--strict"])


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
    unique = (2 if args.distinct_files else 1) * size
    lfs_size = args.lfs_mib * MIB
    required = 2 * size + 2 * unique + (args.versions + 2) * lfs_size + CACHE_BYTES + 10 * GIB
    available = shutil.disk_usage(args.root).free
    runner.check("disk-capacity", available >= required,
                 {"required_bytes": required, "available_bytes": available})
    scratch = runner.run_root / "tmp"
    scratch.mkdir()
    runner.env["TMPDIR"] = str(scratch)
    repo, remote, prefix = runner.prepare_repo("source")
    cap_cache(repo)
    paths = [repo / "model-a.bin", repo / "model-b.bin"]
    write_payload(paths[0], args.file_mib, args.pattern, "large-edits")
    if args.distinct_files:
        write_payload(paths[1], args.file_mib, args.pattern, "large-edits-second")
    else:
        clone_flag = "-c" if sys.platform == "darwin" else "--reflink=always"
        runner.run_cmd("copy-on-write second large file",
                       ["cp", clone_flag, str(paths[0]), str(paths[1])], repo)
    lfs_path = repo / "asset.lfs"
    if lfs_size:
        runner.env["GIT_LFS_SKIP_SMUDGE"] = "1"
        runner.run_crab(repo, ["lfs", "install", "--local", "--skip-repo"])
        runner.run_git(repo, ["lfs", "track", "*.lfs"])
        write_payload(lfs_path, args.lfs_mib, "entropy", "large-lfs")
        runner.run_git(repo, ["add", ".gitattributes"])
    code = repo / "src"
    code.mkdir()
    for index in range(args.code_files):
        (code / f"module-{index:05}.rs").write_text(f"pub const VALUE: u64 = {index};\n")
    (code / "README.txt").write_text("Ordinary small text remains in Git alongside code and large payload pointers.\n")
    runner.check("workload", all(path.stat().st_size == size for path in paths),
                 {"file_bytes": size, "files": 2, "initial_unique_bytes": unique + lfs_size,
                  "logical_payload_bytes": 2 * size + lfs_size, "lfs_bytes": lfs_size,
                  "versions": args.versions, "code_files": args.code_files, "pattern": args.pattern,
                  "first_ingestion": args.first_ingestion})
    history = []
    phases = []

    def record() -> None:
        receipt = runner.artifacts / "large-file-edits.json"
        receipt.write_text(json.dumps({"history": history, "phases": phases, "pattern": args.pattern,
                                      "first_ingestion": args.first_ingestion,
                                      "transport": proxy.snapshot()}, indent=2) + "\n")
        runner.report.artifacts["large_file_edits"] = str(receipt)
        runner.write_report()

    for version in range(args.versions):
        if version:
            offset = (0, size // 2, size - MIB)[version - 1]
            for index, path in enumerate(paths):
                with path.open("r+b") as stream:
                    stream.seek(offset)
                    stream.write(hashlib.shake_256(f"edit:{version}:{index}".encode()).digest(MIB))
            (code / "module-00000.rs").write_text(f"pub const VERSION: u64 = {version};\n")
            if lfs_size:
                with lfs_path.open("r+b") as stream:
                    stream.seek(min(offset, lfs_size - MIB))
                    stream.write(hashlib.shake_256(f"lfs-edit:{version}".encode()).digest(MIB))
        before = proxy.snapshot()
        native_add = (version % 2 == 0) == (args.first_ingestion == "native")
        if not native_add:
            add = runner.run_git(repo, ["add", "--", *[p.name for p in paths]],
                                 name=f"v{version} Git clean-filter add")
        else:
            add = runner.run_crab(repo, ["add", "--jsonl", "*.bin"], name=f"v{version} Crab add")
        staging = repo / ".crab" / "staging"
        local_bytes = {
            name: sum(path.stat().st_size for path in (staging / name).rglob("*") if path.is_file())
            for name in ("segments", "push-plans", "stream-prepared-xorbs")
        }
        # One initial representation plus all edited bytes is sufficient. Allow
        # encoded metadata overhead, while rejecting the observed second full copy.
        max_local_bytes = unique + 2 * version * MIB + max(unique // 10, 32 * MIB)
        runner.check(f"v{version}-no-second-full-local-payload",
                     sum(local_bytes.values()) <= max_local_bytes,
                     {"staging_directory_bytes": local_bytes,
                      "max_bytes": max_local_bytes})
        phases.append({"version": version, "phase": "add", "duration_ms": add.duration_ms,
                       "ingestion": "native" if native_add else "git",
                       "staging_directory_bytes": local_bytes,
                       "transport": RequestCountingProxy.delta(before, proxy.snapshot())})
        for path in paths:
            runner.assert_index_pointer(repo, path.name, size)
        runner.run_git(repo, ["add", "src"])
        if lfs_size:
            runner.run_git(repo, ["add", "asset.lfs"])
            verify_lfs(runner, repo, sha256_file(lfs_path), lfs_size)
        runner.run_git(repo, ["commit", "-m", f"large-file version {version}"])
        before_inventory = object_inventory(runner, ".crab/xorbs/")
        before = proxy.snapshot()
        concurrency = 1 if version == 1 else 4
        push = runner.run_crab(repo, ["push", "--jsonl", "--upload-concurrency", str(concurrency), "origin", "HEAD:refs/heads/main"],
                               name=f"v{version} Crab push")
        transport = RequestCountingProxy.delta(before, proxy.snapshot())
        runner.assert_staging_retired(repo, f"v{version}-retires-published-staging-owners")
        after_inventory = object_inventory(runner, ".crab/xorbs/")
        added_bytes = after_inventory["bytes"] - before_inventory["bytes"]
        phases.append({"version": version, "phase": "push", "duration_ms": push.duration_ms,
                       "new_xorb_bytes": added_bytes, "xorbs": after_inventory, "transport": transport,
                       "upload_concurrency": concurrency})
        expected = {path.name: sha256_file(path) for path in paths + ([lfs_path] if lfs_size else [])}
        history.append({"version": version, "commit": runner.rev_parse(repo, "HEAD"), "files": expected,
                        "code": {str(path.relative_to(repo)): sha256_file(path) for path in code.iterdir()}})
        record()
        runner.check(f"v{version}-published-tip",
                     runner.ls_remote(remote, name=f"v{version} published refs").get("refs/heads/main") == history[-1]["commit"])
        if version:
            runner.check(f"v{version}-incremental-upload", 0 < added_bytes < size // 20,
                         {"new_xorb_bytes": added_bytes, "edited_bytes": 2 * MIB})
        else:
            runner.check("initial-xorb-storage-bound", 0 < added_bytes < unique * 102 // 100,
                         {"new_xorb_bytes": added_bytes, "unique_source_bytes": unique})
        runner.check(f"v{version}-transport-clean", not transport["proxy_errors"], transport)
        if args.lifecycle and version == 0:
            runner.run_crab(repo, ["repack", "--json"], name="retain first-version checkpoint")
    runner.check("v2-root", bool(runner.list_keys(f"{prefix}/v2/root")))
    runner.run_git(repo, ["fsck", "--full", "--strict"])
    # Remove all source authority before proving any historical reconstruction.
    release_verified_run_child(runner, repo.parent)
    release_verified_run_child(runner, runner.cache_dir)
    clone = runner.run_root / "clone"
    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "clone-cache")
    runner.run_cmd("independent clone", [runner.crab_bin, "clone", remote, str(clone)], runner.run_root)
    cap_cache(clone)
    verify_history(runner, proxy, clone, history, phases, "before-maintenance")
    record()
    if args.lifecycle:
        verify_retained_reader(runner, proxy, clone, remote, history)
        qualify_lifecycle(runner, clone, prefix)
        release_verified_run_child(runner, clone)
        runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "post-maintenance-clone-cache")
        runner.run_cmd("post-maintenance independent clone", [runner.crab_bin, "clone", remote, str(clone)], runner.run_root)
        cap_cache(clone)
        verify_history(runner, proxy, clone, history, phases, "after-maintenance")
        record()
    runner.run_git(clone, ["fsck", "--full", "--strict"])
    result = runner.run_crab(clone, ["fsck", "--json"], name="remote integrity")
    runner.report.artifacts["remote_fsck"] = result.stdout_log
    integrity = json.loads(runner.read_stdout(result))["data"]
    runner.check("remote-integrity", integrity["passed"] and integrity["errors"] == 0
                 and integrity["repaired"] == 0 and integrity["repair_failures"] == 0, integrity)
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
    parser.add_argument("--first-ingestion", choices=("native", "git"), default="native")
    parser.add_argument("--distinct-files", action="store_true")
    parser.add_argument("--lfs-mib", type=int, default=0)
    parser.add_argument("--versions", type=int, choices=(2, 4), default=4)
    parser.add_argument("--lifecycle", action="store_true")
    parser.add_argument("--retained-reader-bin", type=Path)
    parser.add_argument("--timeout", type=int, default=7200)
    args = parser.parse_args()
    if (args.file_mib < 16 or args.code_files < 1 or args.lfs_mib < 0 or args.timeout < 1
            or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.run_id)):
        parser.error("require >=16 MiB/file, positive code files and a safe run id")
    if args.lifecycle and (args.retained_reader_bin is None or not args.retained_reader_bin.is_file()):
        parser.error("--lifecycle requires --retained-reader-bin (release qualify_retained_reader example)")
    args.root.mkdir(parents=True, exist_ok=True)
    (args.root / args.run_id).mkdir()
    args.access_key, args.secret_key, args.session_token = "crab", "crab", ""
    args.region, args.push_timeout, args.source = "us-east-1", args.timeout, None
    runner = MeasuredRunner(args)
    proxy = RequestCountingProxy(args.endpoint_url, args.bucket)
    if args.lifecycle:
        args.retained_reader_bin = args.retained_reader_bin.resolve()
        runner.report.artifacts["retained_reader_binary_sha256"] = sha256_file(args.retained_reader_bin)
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
