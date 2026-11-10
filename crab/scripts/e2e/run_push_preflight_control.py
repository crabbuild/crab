#!/usr/bin/env python3
"""Separate process, local preflight and unchanged-ref costs after qualification."""

import argparse
import concurrent.futures
import hashlib
import json
import math
import os
import resource
import subprocess
import threading
import time
from pathlib import Path

from run_concurrent_push_smoke import rustfs_env


def cpu(who):
    usage = resource.getrusage(who)
    return usage.ru_utime + usage.ru_stime


def command(args, repo, env):
    return subprocess.check_output(args, cwd=repo, env=env, text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crab-bin", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.root = args.root.resolve()
    binary = args.crab_bin.resolve()
    helper = args.root / "bin/git-remote-crab"
    assert helper.resolve(strict=True) == binary, "control and fixture helpers must match"
    report = json.loads((args.root / "artifacts/report.json").read_text())
    env = rustfs_env(args.root, report["endpoint_url"], os.environ["AWS_ACCESS_KEY_ID"],
                     os.environ["AWS_SECRET_ACCESS_KEY"], report["env"]["AWS_REGION"])
    assert report["status"] == "ok" and all(c["ok"] for c in report["checks"])
    latest = {row["branch"]: row for row in sorted(report["branch_updates"], key=lambda r: r["round"])}
    assert len(latest) == 50, "preflight control requires 50 qualified writer worktrees"
    writers = [(Path(row["command"]["cwd"]), row["branch"]) for _, row in sorted(latest.items())]
    before = command(["git", "ls-remote", "origin"], writers[0][0], env)
    remote = {name: oid for oid, name in (line.split() for line in before.splitlines())}
    for repo, ref in writers:
        assert command(["git", "rev-parse", "HEAD"], repo, env) == remote[ref]
    rows = []
    result = {
        "scope": "diagnostic only; does not replace real-update latency qualification",
        "version": command([str(binary), "version"], writers[0][0], env),
        "binary_sha256": hashlib.file_digest(binary.open("rb"), "sha256").hexdigest(),
        "cohorts": rows,
    }
    for actors in (1, 10, 50):
        names = ["launch", "version", "dry-run", "unchanged-ref"]
        timings = {name: [] for name in names}
        counters = {name: {"child_cpu_seconds": 0, "parent_cpu_seconds": 0} for name in names}
        for wave in range(100 // actors):
            for name in names[wave % 4:] + names[:wave % 4]:
                barrier = threading.Barrier(actors)

                def run(index):
                    repo, ref = writers[index]
                    argv = {
                        "launch": ["/usr/bin/true"],
                        "version": [str(binary), "version"],
                        "dry-run": [str(binary), "push", "--json", "--dry-run", "origin", f"HEAD:{ref}"],
                        "unchanged-ref": [str(binary), "push", "--json", "origin", f"HEAD:{ref}"],
                    }[name]
                    barrier.wait(timeout=60)
                    started = time.perf_counter_ns()
                    completed = subprocess.run(argv, cwd=repo, env=env, capture_output=True, timeout=120)
                    elapsed = (time.perf_counter_ns() - started) / 1_000_000
                    assert completed.returncode == 0, (name, index, completed.returncode)
                    return elapsed

                child, parent = cpu(resource.RUSAGE_CHILDREN), cpu(resource.RUSAGE_SELF)
                with concurrent.futures.ThreadPoolExecutor(max_workers=actors) as pool:
                    timings[name].extend(pool.map(run, range(actors)))
                counters[name]["child_cpu_seconds"] += cpu(resource.RUSAGE_CHILDREN) - child
                counters[name]["parent_cpu_seconds"] += cpu(resource.RUSAGE_SELF) - parent
        for name, values in timings.items():
            ordered = sorted(values)
            assert len(ordered) == 100
            row = {"actors": actors, "probe": name, "samples": len(ordered),
                   "p50_ms": ordered[math.ceil(len(ordered) * .5) - 1],
                   "p95_ms": ordered[math.ceil(len(ordered) * .95) - 1],
                   "max_ms": max(ordered), **counters[name]}
            rows.append(row)
            print(json.dumps(row), flush=True)
        args.output.write_text(json.dumps(result, indent=2) + "\n")
    assert command(["git", "ls-remote", "origin"], writers[0][0], env) == before
    result["remote_refs_unchanged"] = True
    args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
