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
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path

from qualify import command, compose, issue_path, node_url, prove_node, request_json, run_stage
from render import CONFIG, MEMORY_LIMIT, ROOT, node_name, render


def replica_issue(url: str, index: int) -> tuple[str, int, str]:
    with urllib.request.urlopen(url, timeout=10) as response:
        body = json.load(response)
        if body.get("title") != f"Cell issue on node {index}":
            raise RuntimeError(f"replica returned the wrong issue: {body}")
        reader = response.headers.get("x-crab-cell-reader")
        incarnation = response.headers.get("x-crab-cell-incarnation")
        sequence = int(response.headers.get("x-crab-cell-sequence", "-1"))
        if not reader or len(reader) != 32 or not incarnation or len(incarnation) != 32 or sequence < 1:
            raise RuntimeError("replica response omitted its reader or observed receipt")
        return reader, sequence, incarnation


def prove_readers(port: int, size: int, target: int, index: int = 1, ingress: int = 1) -> dict:
    url = node_url(ingress, port) + issue_path(index) + "/1?read=replica"
    observed: set[str] = set()
    started = time.monotonic()
    deadline = started + 180
    while len(observed) < target and time.monotonic() < deadline:
        for _ in range(target):
            try:
                reader, _, _ = replica_issue(url, index)
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
        reader, sequence, _ = replica_issue(url, index)
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


def measure_reads(port: int, size: int) -> dict:
    result = {}
    for mode in ("owner", "replica"):
        url = node_url(1, port) + issue_path(1) + "/1" + ("?read=replica" if mode == "replica" else "")
        def read(_):
            start = time.monotonic()
            with urllib.request.urlopen(url, timeout=15) as response:
                body = json.load(response)
                if body.get("title") != "Cell issue on node 1":
                    raise RuntimeError("measured read returned the wrong value")
                reader = response.headers.get("x-crab-cell-reader", "owner")
            return time.monotonic() - start, reader
        started = time.monotonic()
        with ThreadPoolExecutor(max_workers=8) as workers:
            samples = list(workers.map(read, range(200)))
        elapsed = time.monotonic() - started
        latencies = sorted(duration * 1000 for duration, _ in samples)
        result[mode] = {
            "requests": len(samples), "concurrency": 8,
            "requests_per_second": round(len(samples) / elapsed, 2),
            "p50_ms": round(latencies[99], 2), "p99_ms": round(latencies[197], 2),
            "reader_counts": dict(Counter(reader for _, reader in samples)),
        }
    return result


def prove_warm_promotion(path: Path, profiles: tuple[str, ...], port: int) -> dict:
    request_json("PUT", node_url(1, port) + "/api/repos/demo/work-19/settings/read-replicas",
                 {"expected_revision": 0, "desired_readers": 2})
    warm = prove_readers(port, 20, 2, 19)
    sessions = {}
    for index in range(1, 21):
        session, _, _ = prove_node(path, profiles, index)
        status = json.loads(compose(path, profiles, "exec", "-T", "node-01", "crab-http-server",
                                    "--config", CONFIG, "cells", "node", "--session", session, "--json"))
        sessions[session] = (index, status["advertisement"]["node"])
    args = ("crab-http-server", "--config", CONFIG, "cells", "status", "--owner", "demo", "--name", "work-19")
    before = json.loads(compose(path, profiles, "exec", "-T", "node-01", *args))
    owner_index = sessions[before["owner"]["session"]][0]
    observer = next(index for index, node in sessions.values()
                    if index != owner_index and node not in warm["reader_counts"])
    started = time.monotonic()
    try:
        compose(path, profiles, "kill", "--signal", "SIGKILL", node_name(owner_index))
        issue = request_json("GET", node_url(observer, port) + issue_path(19) + "/1")
        after = json.loads(compose(path, profiles, "exec", "-T", node_name(observer), *args))
        successor = sessions[after["owner"]["session"]][1]
        if successor not in warm["reader_counts"] or issue["title"] != "Cell issue on node 19":
            raise RuntimeError("failed owner was not replaced by a verified warm reader")
        if after["root"]["commit_sequence"] < before["root"]["commit_sequence"]:
            raise RuntimeError("warm promotion regressed the acknowledged root")
        comment = request_json("POST", node_url(observer, port) + issue_path(19) + "/1/comments",
                               {"body": "written by the promoted warm reader"})
        if comment.get("body") != "written by the promoted warm reader":
            raise RuntimeError("promoted reader did not acknowledge a new mutation")
        return {"old_owner_session": before["owner"]["session"],
                "new_owner_session": after["owner"]["session"], "promoted_reader": successor,
                "previous_readers": warm, "root_before": before["root"], "root_after": after["root"],
                "recovery_and_write_seconds": round(time.monotonic() - started, 3)}
    finally:
        compose(path, profiles, "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300", node_name(owner_index))


def prove_all_reader_loss(path: Path, profiles: tuple[str, ...], stage: dict, port: int, project: str) -> dict:
    policy = request_json(
        "PUT",
        node_url(1, port) + "/api/repos/demo/work-20/settings/read-replicas",
        {"expected_revision": 0, "desired_readers": 2},
    )
    if policy["desired_readers"] != 2:
        raise RuntimeError("all-reader-loss target was not applied")
    before_readers = prove_readers(port, 20, 2, 20)
    node_by_id = {}
    node_by_session = {}
    for index in range(1, 21):
        session, _, _ = prove_node(path, profiles, index)
        status = json.loads(compose(
            path, profiles, "exec", "-T", "node-01", "crab-http-server",
            "--config", CONFIG, "cells", "node", "--session", session, "--json",
        ))
        node_by_id[status["advertisement"]["node"]] = node_name(index)
        node_by_session[session] = node_name(index)
    current = json.loads(compose(path, profiles, "exec", "-T", "node-01", "crab-http-server",
                                 "--config", CONFIG, "cells", "status", "--owner", "demo", "--name", "work-20"))
    owner = node_by_session[current["owner"]["session"]]
    lost = {owner} | {node_by_id[node] for node in before_readers["reader_counts"]}
    if len(lost) != 3:
        raise RuntimeError(f"expected one owner and two distinct readers, got {lost}")
    survivor_index = next(index for index in range(1, 21) if node_name(index) not in lost)
    observer = node_name(survivor_index)
    status_args = (
        "exec", "-T", observer, "crab-http-server", "--config", CONFIG,
        "cells", "status", "--owner", "demo", "--name", "work-20",
    )
    before = json.loads(compose(path, profiles, *status_args))
    started = time.monotonic()
    compose(path, profiles, "kill", "--signal", "SIGKILL", *sorted(lost))
    compose(path, profiles, "rm", "--force", *sorted(lost))
    for service in sorted(lost):
        volume = f"{project}_{service}-data"
        labeled = command("docker", "volume", "inspect", "--format", "{{index .Labels \"com.docker.compose.project\"}}", volume)
        if labeled != project:
            raise RuntimeError(f"refusing to remove an unowned Cell volume: {volume}")
        command("docker", "volume", "rm", volume)
    deadline = time.monotonic() + 180
    after = None
    while time.monotonic() < deadline:
        try:
            issue = request_json("GET", node_url(survivor_index, port) + issue_path(20) + "/1")
            labels = request_json("GET", node_url(survivor_index, port) + "/api/repos/demo/work-20/labels")
            observed = json.loads(compose(path, profiles, *status_args))
            if (
                issue["title"] == "Cell issue on node 20"
                and issue["body"] == "Durable issue created through a constrained Cell node"
                and len(labels.get("items", [])) == 1
                and labels["items"][0]["name"] == "distributed"
                and observed["state"] == "serving"
                and observed["owner"]["session"] != before["owner"]["session"]
                and observed["root"]["commit_sequence"] >= before["root"]["commit_sequence"]
            ):
                after = observed
                break
        except (OSError, KeyError, ValueError, subprocess.CalledProcessError):
            pass
        time.sleep(1)
    if after is None:
        raise RuntimeError("survivor did not recover the acknowledged issue from RustFS")
    replacement = prove_readers(port, 20, 2, 20, survivor_index)
    compose(path, profiles, "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300", *sorted(lost))
    return {
        "lost_nodes": sorted(lost),
        "deleted_local_volumes": sorted(f"{project}_{service}-data" for service in lost),
        "old_owner_session": before["owner"]["session"],
        "new_owner_session": after["owner"]["session"],
        "root_before": before["root"],
        "root_after": after["root"],
        "recovery_seconds": round(time.monotonic() - started, 3),
        "replacement_readers": replacement,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--gateway-port", type=int, default=18080)
    parser.add_argument("--node-port-base", type=int, default=18100)
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()

    command("docker", "info", "--format", "{{.ServerVersion}}")
    label = f"label=com.docker.compose.project={args.project}"
    if any(command("docker", kind, "ls", "-q", "--filter", label) for kind in ("volume", "network")) or command("docker", "ps", "-aq", "--filter", label):
        raise RuntimeError(f"Compose project {args.project} already has resources")
    path = render(args.state, args.project, args.gateway_port, args.node_port_base, True)
    compose(path, (), "config", "--quiet")
    if not args.skip_build:
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
        stage["read_measurement"] = measure_reads(args.node_port_base, size)
        stage["readiness"] = request_json("GET", node_url(1, args.node_port_base) + "/api/repos/demo/work-01/settings/read-replicas")
        stage["reader_resources"] = {}
        for index in range(1, size + 1):
            service = node_name(index)
            stage["reader_resources"][service] = {
                "process_status": compose(path, profiles, "exec", "-T", service, "cat", "/proc/1/status"),
                "descriptors": compose(path, profiles, "exec", "-T", service, "ls", "/proc/1/fd").splitlines(),
                "local_cell_disk_kib": int(compose(path, profiles, "exec", "-T", service, "du", "-sk", "/var/lib/crab/cells").split()[0]),
                "metrics": compose(path, profiles, "exec", "-T", service, "crab-http-server", "--config", CONFIG, "cells", "metrics"),
            }
        report["stages"].append(stage)
        (path.parent / "read-replica-report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"Verified {size} nodes and {target} distinct S3-rooted issue readers", flush=True)
        previous = size
    report["warm_promotion"] = prove_warm_promotion(path, phases[-1][1], args.node_port_base)
    (path.parent / "read-replica-report.json").write_text(json.dumps(report, indent=2) + "\n")
    report["all_reader_loss"] = prove_all_reader_loss(
        path, phases[-1][1], report["stages"][-1], args.node_port_base, args.project
    )
    (path.parent / "read-replica-report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(path.parent / "read-replica-report.json")


if __name__ == "__main__":
    main()
