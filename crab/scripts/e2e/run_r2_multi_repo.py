#!/usr/bin/env python3
"""Measure direct-HTTPS R2 pushes, commit replay, fetches, and verified clones.

Credentials use AWS environment variables. Sources remain read-only. All remote
objects and local evidence are retained; no bucket-wide cleanup is performed.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path

import run_large_repo_rustfs as shared
from run_capsule_k8s_rustfs import sampled_blob_digests


def summary(values: list[int]) -> dict:
    return {
        "count": len(values),
        "mean": round(sum(values) / len(values), 2) if values else None,
        **{name: shared.percentile(values, fraction) for name, fraction in
           (("p50", .50), ("p95", .95), ("p99", .99))},
        "max": max(values, default=0),
    }


def inventory(q: shared.LargeRepositoryQualification) -> dict:
    return {item["Key"]: {"size": int(item["Size"]), "etag": item["ETag"]}
            for item in q.list_remote_objects()}


def pack_bytes(repo: Path) -> int:
    return sum(path.stat().st_size for path in (repo / ".git/objects/pack").glob("*.pack"))


def measure_clone(q, target: Path, expected: str, name: str, cache: str) -> None:
    record = q.run_git(q.run_root,
        ["-c", "protocol.version=2", "clone", "--no-checkout", q.remote_url, str(target)],
        name, timeout=q.args.clone_timeout,
        extra_env={"CRAB_CACHE_DIR": str(q.run_root / cache)})
    actual = q.git_value(target, ["rev-parse", "refs/remotes/origin/main"], name + " tip")
    q.check(name + "-exact-tip", actual == expected)
    q.check(name + "-independent-objects", not (target / ".git/objects/info/alternates").exists())
    q.run_git(target, ["fsck", "--strict", "--full", "--no-reflogs"], name + " fsck",
              timeout=q.args.clone_timeout)
    samples = sampled_blob_digests(str(q.git_bin), target, expected)
    q.check(name + "-sampled-bytes", samples == sampled_blob_digests(str(q.git_bin), q.source, expected))
    size = pack_bytes(target)
    q.report["stages"][name] = {
        "duration_ms": record["duration_ms"], "received_pack_bytes": size,
        "effective_pack_mib_per_second": round(size / 1024**2 / (record["duration_ms"] / 1000), 3),
        "sampled_blobs": len(samples), "tip": actual, "git_fsck": "passed",
        "cache": cache, "telemetry": record["telemetry"],
    }
    q.write_report()


def run_repository(args, source: Path, ordinal: int):
    # Reuse the existing supervisor, immutable binary snapshot, source checks,
    # redaction, and paginated S3 listing. This workload has its own report schema.
    saved_argv = sys.argv
    sys.argv = [saved_argv[0], "--source", str(source), "--root", str(args.root),
        "--run-id", f"{ordinal:02d}-{source.name}", "--bucket", args.bucket,
        "--endpoint-url", os.environ["AWS_ENDPOINT_URL"], "--region", "auto",
        "--crab-bin", args.crab_bin, "--replay-count", str(args.commits),
        "--sample-interval", "0.05", "--retain-worktrees", "--require-existing-bucket"]
    try:
        config = shared.parse_args()
    finally:
        sys.argv = saved_argv
    q = shared.LargeRepositoryQualification(config)
    q.env.update(AWS_ALLOW_HTTP="false", GIT_CONFIG_NOSYSTEM="1",
                 GIT_CONFIG_GLOBAL=os.devnull, GIT_OPTIONAL_LOCKS="0")
    q.report.update(schema="crab.r2-multi-repository.repo", version="1.0", profile="benchmark",
                    valid_for_comparison=False,
                    comparison_invalid_reason="Single shared-host run without a matched idle-host baseline.")
    q.report["measurement_contract"] = {
        "latency": "End-to-end child command wall time, sampled every 50 ms; excludes inventory and fsck.",
        "upload_bytes": "Sizes of new or changed visible objects after each push, not wire bytes or transient writes.",
        "download_bytes": "Local received Git pack sizes, not R2 wire bytes or checkout bytes.",
        "throughput": "Effective bytes divided by complete command wall time, including local Git work.",
        "scope": "Single-client replay; no provider certification, contention, Xet workload, or release gate claim.",
    }
    q.setup()
    try:
        head, base, commits = q.preflight()
        q.report["commit_oids"] = commits
        q.check("full-source-history", q.git_value(q.source, ["rev-parse", "--is-shallow-repository"], "source shallow") == "false")
        promisor = q.run_git(q.source, ["config", "--local", "--get-regexp",
            r"^(extensions\.partialclone|remote\..*\.(promisor|partialclonefilter))$"],
            "source promisor check", check=False)
        q.check("no-promisor-source", promisor["exit_code"] == 1)
        q.run_cmd("verify R2 bucket", [str(q.aws_bin), "--endpoint-url", config.endpoint_url,
            "s3api", "head-bucket", "--bucket", args.bucket], q.run_root)
        previous = inventory(q)
        q.check("isolated-remote-prefix", not previous)
        q.report["provenance"]["object_store"] = {"kind": "cloudflare-r2", "region": "auto", "https": True}
        q.setup_replay(base)
        for i, commit in enumerate([base, *commits]):
            record = q.push_commit(commit, i, f"push-{i:05d}")
            events = [json.loads(line) for line in q.stdout(record).splitlines() if line.strip()]
            result = events[-1].get("data", {})
            q.check(f"push-result-{i}", result.get("refs_pushed") == 1
                    and all(ref["status"] == "ok" for ref in result.get("refs", [])))
            current = inventory(q)
            visible_bytes = sum(item["size"] for key, item in current.items() if previous.get(key) != item)
            q.report["pushes"][-1].update(
                new_or_changed_visible_bytes=visible_bytes,
                effective_visible_mib_per_second=round(visible_bytes / 1024**2 / (record["duration_ms"] / 1000), 3))
            previous = current
            q.write_report()
            print(f"{source.name}: push {i}/{len(commits)} {record['duration_ms']} ms", flush=True)
            if i == 0:
                measure_clone(q, q.incremental_clone, base, "seed-clone", "cache-consumer")
            elif i % args.interval == 0 or i == len(commits):
                before = pack_bytes(q.incremental_clone)
                fetched = q.run_git(q.incremental_clone, ["-c", "protocol.version=2", "fetch", "origin"],
                    f"fetch-{i:05d}", timeout=config.clone_timeout,
                    extra_env={"CRAB_CACHE_DIR": str(q.run_root / "cache-consumer")})
                tip = q.git_value(q.incremental_clone, ["rev-parse", "refs/remotes/origin/main"], f"fetch tip {i}")
                q.check(f"fetch-exact-tip-{i}", tip == commit)
                q.run_git(q.incremental_clone, ["fsck", "--strict", "--full"], f"fetch fsck {i}", timeout=config.clone_timeout)
                q.report["stages"][f"fetch-{i:05d}"] = {
                    "duration_ms": fetched["duration_ms"], "tip": tip,
                    "pack_storage_delta_bytes": pack_bytes(q.incremental_clone) - before,
                    "git_fsck": "passed", "telemetry": fetched["telemetry"]}
                q.write_report()
        q.run_crab(q.replay_repo, ["fsck", "--jsonl"], "remote Crab fsck", timeout=config.clone_timeout)
        q.report["correctness"]["remote_crab_fsck"] = "passed"
        measure_clone(q, q.clone_root / "cold", head, "cold-clone", "cache-final")
        measure_clone(q, q.clone_root / "warm", head, "warm-clone", "cache-final")
        q.run_git(q.incremental_clone, ["fsck", "--strict", "--full"], "final incremental fsck", timeout=config.clone_timeout)
        q.check("incremental-sampled-bytes", sampled_blob_digests(str(q.git_bin), q.incremental_clone, head)
                == sampled_blob_digests(str(q.git_bin), q.source, head))
        q.check("capsule-v2-storage", any("/v2/capsules/" in key for key in previous)
                and any("/v2/refs/" in key for key in previous))
        q.report["store_snapshots"].append({"stage": "final", "objects": len(previous),
            "bytes": sum(item["size"] for item in previous.values())})
        q.verify_source_unchanged()
        q.check("frozen-binary-unchanged", hashlib.sha256(q.crab_bin.read_bytes()).hexdigest()
                == q.report["provenance"]["crab_binary_sha256"])
        pushes = q.report["pushes"][1:]
        total_ms = sum(item["duration_ms"] for item in pushes)
        fetches = [stage for name, stage in q.report["stages"].items() if name.startswith("fetch-")]
        q.report["metrics"] = {
            "incremental_push_latency_ms": summary([item["duration_ms"] for item in pushes]),
            "commits_per_second": round(len(pushes) / (total_ms / 1000), 4),
            "incremental_effective_visible_mib_per_second": round(sum(item["new_or_changed_visible_bytes"] for item in pushes) / 1024**2 / (total_ms / 1000), 4),
            "fetch_latency_ms": summary([item["duration_ms"] for item in fetches]),
        }
        q.report["status"] = "ok"
    except Exception as error:
        q.report["status"] = "failed"
        q.report["error"] = str(error)
        try:
            q.verify_source_unchanged()
        except Exception as source_error:
            q.report["source_verification_error"] = str(source_error)
        print(f"{source.name}: FAILED: {error}", flush=True)
    finally:
        q.report["finished_at"] = shared.utc_now()
        q.write_report()
    return q


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, action="append", required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--crab-bin", required=True)
    parser.add_argument("--commits", type=int, default=100)
    parser.add_argument("--interval", type=int, default=25)
    args = parser.parse_args()
    if args.commits < 1 or args.interval < 1:
        parser.error("commits and interval must be positive")
    if not os.environ.get("AWS_ENDPOINT_URL", "").startswith("https://"):
        parser.error("AWS_ENDPOINT_URL must use HTTPS")
    runs = [run_repository(args, source, i) for i, source in enumerate(args.source, 1)]
    for q in runs:
        if q.report["status"] != "ok":
            continue
        try:
            tip = q.git_value(q.replay_repo, ["ls-remote", "origin", "refs/heads/main"], "cross-repository isolation tip").split()[0]
            q.check("tip-preserved-after-other-repositories", tip == q.report["source"]["revision"])
        except Exception as error:
            q.report.update(status="failed", error=str(error))
        q.write_report()
    combined = {"schema": "crab.r2-multi-repository", "version": "1.0", "bucket": args.bucket,
        "finished_at": shared.utc_now(), "repositories": [q.report for q in runs]}
    combined["status"] = "ok" if all(q.report["status"] == "ok" for q in runs) else "failed"
    (args.root / "report.json").write_text(json.dumps(combined, indent=2, sort_keys=True) + "\n")
    return 0 if combined["status"] == "ok" else 1


if __name__ == "__main__":
    raise SystemExit(main())
