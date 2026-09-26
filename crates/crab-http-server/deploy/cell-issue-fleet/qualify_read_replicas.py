#!/usr/bin/env python3
"""Qualify S3-rooted issue read replicas through 3, 5, 10, and 20 local nodes."""

import argparse
import hashlib
import json
import subprocess
import time
import urllib.error
import urllib.request
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

from qualify import command, compose, issue_path, node_url, request_json, run_stage
from render import MEMORY_LIMIT, ROOT, render


def replica_issue(url: str) -> tuple[str, int, str]:
    with urllib.request.urlopen(url, timeout=10) as response:
        body = json.load(response)
        if body.get("title") != "Cell issue on node 1":
            raise RuntimeError(f"replica returned the wrong issue: {body}")
        reader = response.headers.get("x-crab-cell-reader")
        incarnation = response.headers.get("x-crab-cell-incarnation")
        sequence = int(response.headers.get("x-crab-cell-sequence", "-1"))
        if not reader or len(reader) != 32 or not incarnation or len(incarnation) != 32 or sequence < 1:
            raise RuntimeError("replica response omitted its reader or observed receipt")
        return reader, sequence, incarnation


def prove_readers(port: int, size: int, target: int) -> dict:
    url = node_url(1, port) + issue_path(1) + "/1?read=replica"
    observed: set[str] = set()
    started = time.monotonic()
    deadline = started + 180
    while len(observed) < target and time.monotonic() < deadline:
        for _ in range(target):
            try:
                reader, _, _ = replica_issue(url)
                observed.add(reader)
            except (OSError, urllib.error.HTTPError, ValueError):
                pass
        if len(observed) < target:
            time.sleep(1)
    if len(observed) != target:
        raise RuntimeError(f"{size} nodes: only {len(observed)}/{target} distinct readers served")

    counts: Counter[str] = Counter()
    sequences: list[int] = []
    for _ in range(target * 10):
        reader, sequence, _ = replica_issue(url)
        counts[reader] += 1
        sequences.append(sequence)
    if len(counts) != target:
        raise RuntimeError(f"{size} nodes: read distribution lost a selected reader")
    return {
        "desired": target,
        "observed_readers": len(observed),
        "convergence_seconds": round(time.monotonic() - started, 3),
        "reader_counts": dict(sorted(counts.items())),
        "min_sequence": min(sequences),
        "max_sequence": max(sequences),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--gateway-port", type=int, default=18080)
    parser.add_argument("--node-port-base", type=int, default=18100)
    args = parser.parse_args()

    command("docker", "info", "--format", "{{.ServerVersion}}")
    label = f"label=com.docker.compose.project={args.project}"
    if any(command("docker", kind, "ls", "-q", "--filter", label) for kind in ("volume", "network")) or command("docker", "ps", "-aq", "--filter", label):
        raise RuntimeError(f"Compose project {args.project} already has resources")
    path = render(args.state, args.project, args.gateway_port, args.node_port_base, True)
    compose(path, (), "config", "--quiet")
    subprocess.run(["docker", "compose", "--file", str(path), "build", "release-init"], check=True)
    source_diff = command("git", "-C", str(ROOT), "diff", "--binary", "HEAD")
    report = {
        "project": args.project,
        "source_commit": command("git", "-C", str(ROOT), "rev-parse", "HEAD"),
        "source_diff_sha256": hashlib.sha256(source_diff.encode()).hexdigest(),
        "image": command("docker", "image", "inspect", "--format", "{{.Id}}", f"{args.project}:local"),
        "started_at": datetime.now(timezone.utc).isoformat(),
        "profile": "local-rustfs-object-read-replicas",
        "node_cpu_limit": 1,
        "node_memory_limit_bytes": MEMORY_LIMIT,
        "stages": [],
    }
    phases = [(3, ()), (5, ("five",)), (10, ("five", "ten")), (20, ("five", "ten", "twenty"))]
    previous = 0
    revision = 0
    for size, profiles in phases:
        stage = run_stage(path, profiles, previous, size, args.gateway_port, args.node_port_base)
        target = size - 1
        policy = request_json(
            "PUT",
            node_url(1, args.node_port_base) + "/api/repos/demo/work-01/settings/read-replicas",
            {"expected_revision": revision, "desired_readers": target},
        )
        revision = policy["revision"]
        if policy["desired_readers"] != target:
            raise RuntimeError(f"{size} nodes: target update was not applied")
        stage["replicas"] = prove_readers(args.node_port_base, size, target)
        report["stages"].append(stage)
        (path.parent / "read-replica-report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"Verified {size} nodes and {target} distinct S3-rooted issue readers", flush=True)
        previous = size
    print(path.parent / "read-replica-report.json")


if __name__ == "__main__":
    main()
