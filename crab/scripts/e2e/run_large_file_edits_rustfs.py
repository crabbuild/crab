#!/usr/bin/env python3
"""Qualify repeated 10-GiB edits through both Git clean and native Crab push.

Two copy-on-write files start with identical, non-sparse deterministic entropy.
Edits diverge at the beginning, middle and end. Each committed version must
hydrate byte-identically from a fresh cache after the source repository is removed.
Only this invocation's disposable data is released; logs and receipts remain.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import threading
from pathlib import Path

from run_add_commit_push_rustfs_smoke import AddCommitPushSmoke, REPO_ROOT, sha256_file
from run_add_push_scale_rustfs import object_inventory, release_verified_run_child
from run_concurrent_push_smoke import RequestCountingProxy

MIB = 1024**2
GIB = 1024**3
CACHE_BYTES = 2 * GIB


class MeasuredRunner(AddCommitPushSmoke):
    def sample_resources(self, stop, peaks, cwd):
        while not stop.is_set():
            try:
                probe = subprocess.Popen(["ps", "-axo", "pid=,ppid=,rss="],
                                         stdout=subprocess.PIPE, text=True)
                output, _ = probe.communicate()
                if probe.returncode:
                    raise RuntimeError("process-tree sampling failed")
                rows = [tuple(map(int, row.split())) for row in output.splitlines()]
                descendants = {os.getpid()}
                while True:
                    children = {pid for pid, parent, _ in rows if parent in descendants
                                and pid != probe.pid}
                    if children <= descendants:
                        break
                    descendants.update(children)
                rss = sum(kib * 1024 for pid, _, kib in rows
                          if pid in descendants and pid != os.getpid())
                if rss:
                    peaks["process_tree_peak_rss_bytes"] = max(
                        peaks.get("process_tree_peak_rss_bytes", 0), rss)
                staging = cwd / ".crab" / "staging"
                payload_bytes = 0
                sampled_files = set()
                for name in ("segments", "push-plans", "stream-prepared-xorbs"):
                    for path in (staging / name).rglob("*"):
                        try:
                            if path.is_file():
                                metadata = path.stat()
                                identity = (metadata.st_dev, metadata.st_ino)
                                if identity not in sampled_files:
                                    sampled_files.add(identity)
                                    payload_bytes += metadata.st_size
                        except FileNotFoundError:
                            pass  # Installation/retirement can move a sampled file.
                peaks["peak_staging_payload_bytes"] = max(
                    peaks.get("peak_staging_payload_bytes", 0), payload_bytes)
            except Exception as error:
                peaks.setdefault("sampling_errors", []).append(str(error))
                return
            stop.wait(0.25)

    def run_cmd(self, name, args, cwd, **kwargs):
        timed = args[0] in (self.crab_bin, "git")
        peaks = {"resource_sampling_interval_seconds": 0.25}
        stop = threading.Event()
        monitor = None
        if timed:
            args = ["/usr/bin/time", "-l" if sys.platform == "darwin" else "-v", *args]
            monitor = threading.Thread(target=self.sample_resources,
                                       args=(stop, peaks, cwd), daemon=True)
            monitor.start()
            kwargs["extra_env"] = {**(kwargs.get("extra_env") or {}),
                                   "CRAB_LOG": "warn,crab_staging::ingest=debug"}
        try:
            result = super().run_cmd(name, args, cwd, **kwargs)
        finally:
            stop.set()
            if monitor:
                monitor.join()
        if timed:
            stderr = Path(result.stderr_log).read_text()
            stderr = re.sub(r"\x1b\[[0-9;]*m", "", stderr)
            written_bytes = 0
            phases = {}
            for line in stderr.splitlines():
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                fields = event.get("fields", {}) if isinstance(event, dict) else {}
                if fields.get("message") == "staging prepared payload written":
                    written_bytes += int(fields["payload_bytes"])
                if fields.get("message") == "ingestion timings":
                    for key, value in fields.items():
                        if key.endswith("_ms") or key.startswith("input_bytes"):
                            phases[key] = phases.get(key, 0) + int(value)
            peaks["prepared_payload_file_bytes_written"] = written_bytes
            peaks["ingestion_phases"] = phases
            self.report.commands[-1].update(peaks)
            pattern = (r"(\d+)\s+maximum resident set size" if sys.platform == "darwin"
                       else r"Maximum resident set size \(kbytes\):\s*(\d+)")
            match = re.search(pattern, stderr)
            if match:
                self.report.commands[-1]["max_rss_bytes"] = int(match[1]) * (1 if sys.platform == "darwin" else 1024)
            self.write_report()
            self.check(f"{name}-resource-sampling", not peaks.get("sampling_errors"), peaks)
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
    with paths[0].open("wb") as output:
        for block in range(args.file_mib):
            output.write(hashlib.shake_256(f"large-edits:{block}".encode()).digest(MIB))
    clone_flag = "-c" if sys.platform == "darwin" else "--reflink=always"
    runner.run_cmd("copy-on-write second large file",
                   ["cp", clone_flag, str(paths[0]), str(paths[1])], repo)
    code = repo / "src"
    code.mkdir()
    for index in range(args.code_files):
        (code / f"module-{index:05}.rs").write_text(f"pub const VALUE: u64 = {index};\n")
    runner.check("workload", all(path.stat().st_size == size for path in paths),
                 {"file_bytes": size, "files": 2, "initial_unique_bytes": size,
                  "versions": 4, "code_files": args.code_files})
    history = []
    phases = []

    def record() -> None:
        receipt = runner.artifacts / "large-file-edits.json"
        receipt.write_text(json.dumps({"history": history, "phases": phases,
                                      "transport": proxy.snapshot()}, indent=2) + "\n")
        runner.report.artifacts["large_file_edits"] = str(receipt)
        runner.write_report()

    for version in range(4):
        edit_bytes = min(MIB, size // 2048)
        if version:
            offset = (0, size // 2, size - edit_bytes)[version - 1]
            for index, path in enumerate(paths):
                with path.open("r+b") as stream:
                    stream.seek(offset)
                    stream.write(hashlib.shake_256(f"edit:{version}:{index}".encode()).digest(edit_bytes))
            (code / "module-00000.rs").write_text(f"pub const VERSION: u64 = {version};\n")
        before = proxy.snapshot()
        git_add = bool((version + (args.start_with == "clean")) % 2)
        if git_add:
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
        if version in (0, 3):
            before_unchanged = local_bytes
            if git_add:
                for path in paths:
                    stat = path.stat()
                    os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns + 2_000_000_000))
                unchanged = runner.run_crab(repo, ["add", "--jsonl", "*.bin"],
                                            name=f"v{version} unchanged native add")
            else:
                unchanged = runner.run_git(repo, ["add", "--renormalize", "--", *[p.name for p in paths]],
                                           name=f"v{version} unchanged Git clean add")
            after_unchanged = {
                name: sum(path.stat().st_size for path in (staging / name).rglob("*") if path.is_file())
                for name in ("segments", "push-plans")
            }
            command = next(record for record in reversed(runner.report.commands)
                                          if record["stdout_log"] == unchanged.stdout_log)
            runner.check(f"v{version}-unchanged-no-payload-write",
                         command.get("prepared_payload_file_bytes_written") == 0
                         and all(after_unchanged[name] <= before_unchanged[name] for name in after_unchanged),
                         {"before": before_unchanged, "after": after_unchanged})
            phases.append({"version": version, "phase": "unchanged-add", "duration_ms": unchanged.duration_ms,
                           "staging_directory_bytes": after_unchanged})
            for path in paths:
                runner.assert_index_pointer(repo, path.name, size)
        runner.run_git(repo, ["add", "src"])
        runner.run_git(repo, ["commit", "-m", f"large-file version {version}"])
        if version == 0 and args.pre_push_edit:
            history.append({"version": "initial", "commit": runner.rev_parse(repo, "HEAD"),
                            "files": {path.name: sha256_file(path) for path in paths}})
            for index, path in enumerate(paths):
                with path.open("r+b") as stream:
                    stream.seek(size - edit_bytes)
                    stream.write(hashlib.shake_256(f"prepush:{index}".encode()).digest(edit_bytes))
            late = runner.run_git(repo, ["add", "--", *[p.name for p in paths]],
                                  name="pre-push late Git clean edit")
            command = next(record for record in reversed(runner.report.commands)
                           if record["stdout_log"] == late.stdout_log)
            written = command.get("prepared_payload_file_bytes_written", 0)
            runner.check("pre-push-late-edit-bounded-payload-write", 0 < written < 4 * edit_bytes + MIB,
                         {"written_bytes": written, "edited_bytes": 2 * edit_bytes})
            for path in paths:
                runner.assert_index_pointer(repo, path.name, size)
            runner.run_git(repo, ["commit", "-m", "late edit before first push"])
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
                         {"new_xorb_bytes": added_bytes, "edited_bytes": 2 * edit_bytes})
        else:
            runner.check("initial-identical-files-share-xorbs", 0 < added_bytes < size * 102 // 100 + (4 * edit_bytes + MIB if args.pre_push_edit else 0),
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
    parser.add_argument("--start-with", choices=("native", "clean"), default="native")
    parser.add_argument("--pre-push-edit", action="store_true",
                        help="Commit a late edit before the first push and reconstruct both versions")
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
