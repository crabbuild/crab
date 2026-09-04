#!/usr/bin/env python3
"""Pair byte-verified warm-cache Git LFS checkouts across two release binaries."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import shlex
import signal
import statistics
import subprocess
import time

from run_protocol_v2_partial_clone_rustfs_smoke import sha256_file, utc_now


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--size-mib", type=int, default=256)
    parser.add_argument("--pairs", type=int, default=8)
    args = parser.parse_args()
    if args.size_mib < 1 or args.pairs < 2:
        parser.error("size must be positive and at least two pairs are required")
    root = (args.root / args.run_id).resolve()
    workspace = (Path.home() / "Workspace").resolve()
    if not root.is_relative_to(workspace) or root == workspace:
        raise RuntimeError("qualification root must be beneath the workspace volume")
    root.mkdir()
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_TERMINAL_PROMPT": "0", "CRAB_CACHE_DIR": str(root / "cache")})
    report = {"schema": "crab.lfs-checkout-comparison", "status": "running", "started_at": utc_now(),
              "scope": "paired warm-cache local checkout diagnostic, not a release SLO verdict",
              "valid_for_comparison": False,
              "performance_verdict": "not_qualified",
              "comparison_limit": "Desktop host is not isolated; no cold-cache, remote, memory or tail-latency claim.",
              "host": {"platform": platform.platform(), "cpu_count": os.cpu_count(),
                       "python": platform.python_version(), "load_start": os.getloadavg()},
              "driver_sha256": sha256_file(Path(__file__)),
              "helper_sha256": sha256_file(Path(__file__).with_name("run_protocol_v2_partial_clone_rustfs_smoke.py")),
              "binaries": {}, "commands": [], "samples": [], "summary": {}}

    def run(command, cwd, timed=False):
        index = len(report["commands"])
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        started = time.perf_counter()
        process = subprocess.Popen(command, cwd=cwd, env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, start_new_session=True)
        try:
            stdout, stderr = process.communicate(timeout=60)
            exit_code = process.returncode
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate()
            exit_code = -124
        elapsed = time.perf_counter() - started
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        (root / f"{index:03d}.stdout").write_bytes(stdout)
        (root / f"{index:03d}.stderr").write_bytes(stderr)
        result = {"args": [str(arg) for arg in command], "exit_code": exit_code, "timed": timed,
                  "wall_seconds": elapsed, "child_user_seconds": after.ru_utime - before.ru_utime,
                  "child_system_seconds": after.ru_stime - before.ru_stime}
        report["commands"].append(result)
        if exit_code:
            raise RuntimeError(f"command {index} failed with exit {exit_code}")
        return stdout, result

    try:
        version, _ = run(["git", "--version"], root)
        report["git_version"] = version.decode().strip()
        for label, binary in binaries.items():
            version, _ = run([binary, "version", "--json"], root)
            report["binaries"][label] = {"sha256": sha256_file(binary), "metadata": json.loads(version)["data"]}
        fixture = root / "fixture.bin"
        block = hashlib.shake_256(b"crab paired LFS checkout fixture").digest(1024 * 1024)
        with fixture.open("wb") as output:
            for _ in range(args.size_mib):
                output.write(block)
        oid = sha256_file(fixture)
        size = fixture.stat().st_size
        report["fixture"] = {"sha256": oid, "size": size}
        pointer = f"version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n"
        for mode, suffix in (("native", ["filter-process"]), ("lfs", ["lfs", "filter-process"]),
                             ("standalone", ["lfs", "smudge"])):
            repo = root / mode
            run(["git", "init", "--quiet", str(repo)], root)
            git = ["git", "-C", str(repo)]
            for key, value in (("user.name", "Qualification"), ("user.email", "qualification@example.invalid"),
                               ("filter.lfs.required", "true"), ("filter.lfs.clean", "cat")):
                run(git + ["config", key, value], root)
            (repo / ".gitattributes").write_text("asset.bin filter=lfs -text\n")
            asset = repo / "asset.bin"
            asset.write_text(pointer)
            run(git + ["add", ".gitattributes", "asset.bin"], root)
            run(git + ["commit", "-qm", "LFS pointer fixture"], root)
            cache = repo / ".git/lfs/objects" / oid[:2] / oid[2:4] / oid
            cache.parent.mkdir(parents=True)
            os.link(fixture, cache)
            key = "filter.lfs.smudge" if mode == "standalone" else "filter.lfs.process"
            # Round zero warms both binaries. Later pairs alternate AB/BA to
            # reduce order bias; fixture hashing prewarms the same cache input.
            for pair in range(args.pairs + 1):
                labels = ("baseline", "candidate") if pair % 2 == 0 else ("candidate", "baseline")
                for label in labels:
                    run(git + ["config", key, shlex.join([str(binaries[label]), *suffix])], root)
                    asset.write_bytes(b"force checkout conversion")
                    if sha256_file(cache) != oid:
                        raise RuntimeError("input cache changed")
                    _, result = run(git + ["checkout-index", "--force", "--all"], root, timed=True)
                    exact = asset.stat().st_size == size and sha256_file(asset) == oid
                    report["samples"].append({"mode": mode, "binary": label, "pair": pair,
                                              "warmup": pair == 0, "exact_bytes": exact,
                                              "wall_seconds": result["wall_seconds"],
                                              "cpu_seconds": result["child_user_seconds"] + result["child_system_seconds"],
                                              "load": os.getloadavg()})
                    if not exact:
                        raise RuntimeError("checkout output differs from the fixture")
            summary = {}
            for label in binaries:
                samples = [s for s in report["samples"] if s["mode"] == mode and s["binary"] == label and not s["warmup"]]
                summary[label] = {"median_wall_seconds": statistics.median(s["wall_seconds"] for s in samples),
                                  "median_cpu_seconds": statistics.median(s["cpu_seconds"] for s in samples),
                                  "samples": len(samples)}
            for metric in ("wall", "cpu"):
                key = f"median_{metric}_seconds"
                summary[f"candidate_to_baseline_{metric}_ratio"] = summary["candidate"][key] / summary["baseline"][key]
            report["summary"][mode] = summary
        for label, binary in binaries.items():
            if sha256_file(binary) != report["binaries"][label]["sha256"]:
                raise RuntimeError(label + " binary changed")
        report["status"] = "passed"
    except Exception as error:
        report["status"] = "failed"
        report["error"] = str(error)
    finally:
        report["finished_at"] = utc_now()
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "error": report.get("error"), "report": str(root / "report.json")}))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
