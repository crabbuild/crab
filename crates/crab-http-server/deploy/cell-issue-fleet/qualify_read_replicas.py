#!/usr/bin/env python3
"""Qualify S3-rooted issue read replicas through 3, 5, 10, and 20 local nodes."""

import argparse
import hashlib
import json
import math
import re
import subprocess
import time
import urllib.error
import urllib.request
import uuid
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path

from qualify import command, compose, issue_path, node_url, prove_node, request_json, run_stage
from render import CONFIG, MEMORY_LIMIT, ROOT, RUSTFS_NOFILE_LIMIT, node_name, render


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


# These are bounded runtime counters, not all provider HTTP requests. In
# particular, membership/policy reads and provider-internal retries are excluded.
COST_METRICS = (
    "crab_cell_control_reads_total",
    "crab_cell_ltx_origin_requests_total",
    "crab_cell_ltx_origin_bytes_total",
    "crab_cell_ltx_logical_reads_total",
)


def parse_cost_metrics(text: str) -> dict[str, float]:
    counters = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        match = re.fullmatch(r'(\w+(?:\{[^}]*\})?)\s+([^\s]+)', line)
        if not match or match[1].split("{", 1)[0] not in COST_METRICS:
            continue
        value = float(match[2])
        if not math.isfinite(value) or value < 0 or match[1] in counters:
            raise RuntimeError("invalid or duplicate runtime cost counter")
        counters[match[1]] = value
    missing = set(COST_METRICS) - {key.split("{", 1)[0] for key in counters}
    if missing:
        raise RuntimeError(f"runtime cost counters missing: {sorted(missing)}")
    return counters


def cost_snapshot(path: Path, profiles: tuple[str, ...], size: int) -> dict:
    def sample(index):
        started = time.monotonic()
        service = node_name(index)
        metrics = compose(path, profiles, "exec", "-T", service, "crab-http-server",
                          "--config", CONFIG, "cells", "metrics")
        return service, {"started_seconds": started, "finished_seconds": time.monotonic(),
                         "counters": parse_cost_metrics(metrics)}
    with ThreadPoolExecutor(max_workers=8) as workers:
        return dict(workers.map(sample, range(1, size + 1)))


def cost_delta(before: dict, after: dict) -> dict:
    if before.keys() != after.keys():
        raise RuntimeError("node set changed during the measurement")
    nodes = {}
    totals = dict.fromkeys(COST_METRICS, 0.0)
    for node, start in before.items():
        end = after[node]
        if start["counters"].keys() != end["counters"].keys():
            raise RuntimeError(f"{node}: counter series changed during the measurement")
        delta = {key: end["counters"][key] - value for key, value in start["counters"].items()}
        if any(value < 0 for value in delta.values()):
            raise RuntimeError(f"{node}: counter reset during the measurement")
        for key, value in delta.items():
            totals[key.split("{", 1)[0]] += value
        nodes[node] = {"counters": delta,
                       "window_seconds": round(end["finished_seconds"] - start["started_seconds"], 6)}
    return {"before": before, "after": after, "nodes": nodes, "totals": totals,
            "scope": "whole-node windows including background work and metric collection; "
                     "control loads and LTX fetches only, not total S3 billing requests"}


def measure_refresh(path: Path, profiles: tuple[str, ...], port: int, size: int,
                    readers: set[str], owner: str) -> dict:
    url = node_url(1, port) + issue_path(1) + "/1"
    current = request_json("GET", url)
    _, _, incarnation = replica_issue(url + "?read=replica", 1)
    before = cost_snapshot(path, profiles, size)
    body = f"Acknowledged S3-rooted refresh at {size} nodes"
    changed = request_json("PATCH", url, {"version": current["version"], "body": body})
    acknowledged = time.monotonic()
    if changed.get("body") != body:
        raise RuntimeError("refresh mutation did not return its acknowledged value")
    control = json.loads(compose(path, profiles, "exec", "-T", "node-01", "crab-http-server",
                                 "--config", CONFIG, "cells", "status", "--owner", "demo", "--name", "work-01"))
    minimum = control["root"]["commit_sequence"]
    ready = {}
    max_sequence_lag = 0
    unavailable = 0
    deadline = acknowledged + 180
    while set(ready) != readers and time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url + "?read=replica", timeout=10) as response:
                observed = json.load(response)
                node = response.headers.get("x-crab-cell-reader")
                sequence = int(response.headers.get("x-crab-cell-sequence", "-1"))
                if node not in readers or response.headers.get("x-crab-cell-incarnation") != incarnation or sequence < 1:
                    raise RuntimeError("refresh read returned an unexpected reader or incarnation")
                max_sequence_lag = max(max_sequence_lag, minimum - sequence)
                if sequence >= minimum:
                    if observed.get("body") != body:
                        raise RuntimeError("replica receipt covers the write but its value is stale")
                    ready.setdefault(node, {"observed_sequence": sequence,
                                            "observed_after_ack_seconds": round(time.monotonic() - acknowledged, 6)})
        except urllib.error.HTTPError as error:
            if error.code != 503:
                raise
            unavailable += 1
        if set(ready) != readers:
            time.sleep(0.02)
    if set(ready) != readers:
        raise RuntimeError(f"refresh reached only {len(ready)}/{len(readers)} selected readers")
    cost = cost_delta(before, cost_snapshot(path, profiles, size))
    reader_bytes = sum(value for node, values in cost["nodes"].items() if node != owner
                       for key, value in values["counters"].items()
                       if key.split("{", 1)[0] == "crab_cell_ltx_origin_bytes_total")
    return {"body": body, "authority_root_after_ack": control["root"], "incarnation": incarnation,
            "owner_node": owner, "readers": ready, "max_observed_sequence_lag": max_sequence_lag,
            "unavailable_attempts": unavailable, "cost": cost,
            "reader_node_ltx_bytes_during_refresh": reader_bytes,
            "timing_scope": "poll-observed upper bounds after acknowledgement, including root inspection"}


def measure_reads(path: Path, profiles: tuple[str, ...], port: int, size: int) -> dict:
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
        before = cost_snapshot(path, profiles, size)
        started = time.monotonic()
        with ThreadPoolExecutor(max_workers=8) as workers:
            samples = list(workers.map(read, range(200)))
        elapsed = time.monotonic() - started
        latencies = sorted(duration * 1000 for duration, _ in samples)
        cost = cost_delta(before, cost_snapshot(path, profiles, size))
        cost["amortized_per_completed_read"] = {key: value / len(samples) for key, value in cost["totals"].items()}
        result[mode] = {
            "requests": len(samples), "concurrency": 8,
            "requests_per_second": round(len(samples) / elapsed, 2),
            "p50_ms": round(latencies[99], 2), "p99_ms": round(latencies[197], 2),
            "reader_counts": dict(Counter(reader for _, reader in samples)),
            "cost": cost,
        }
    return result


def set_reader_target(port: int, repository: int, desired: int) -> dict:
    # Earlier fault phases may have killed this Cell's owner on a shared node.
    request_json("GET", node_url(1, port) + issue_path(repository) + "/1")
    url = node_url(1, port) + f"/api/repos/demo/work-{repository:02d}/settings/read-replicas"
    current = request_json("GET", url)
    if current["desired_readers"] == desired and not current["stale_incarnation"]:
        return current
    return request_json("PUT", url,
                        {"expected_revision": current["revision"], "desired_readers": desired})


def node_inventory(path: Path, profiles: tuple[str, ...], size: int) -> dict:
    sessions = {}
    for index in range(1, size + 1):
        session, _, _ = prove_node(path, profiles, index)
        status = json.loads(compose(path, profiles, "exec", "-T", "node-01", "crab-http-server",
                                    "--config", CONFIG, "cells", "node", "--session", session, "--json"))
        sessions[session] = (index, status["advertisement"]["node"])
    return sessions


def prove_reader_replacement(path: Path, profiles: tuple[str, ...], port: int) -> dict:
    set_reader_target(port, 18, 2)
    before_readers = prove_readers(port, 20, 2, 18)
    sessions = node_inventory(path, profiles, 20)
    lost_index, lost_node = next((index, node) for index, node in sessions.values()
                                 if index != 1 and node in before_readers["reader_counts"])
    args = ("exec", "-T", "node-01", "crab-http-server", "--config", CONFIG,
            "cells", "status", "--owner", "demo", "--name", "work-18")
    before = json.loads(compose(path, profiles, *args))
    started = time.monotonic()
    try:
        compose(path, profiles, "kill", "--signal", "SIGKILL", node_name(lost_index))
        replacement = prove_readers(port, 20, 2, 18)
        after = json.loads(compose(path, profiles, *args))
        if lost_node in replacement["reader_counts"]:
            raise RuntimeError("dead reader remained in the ready set")
        if after["owner"] != before["owner"] or after["epoch"] != before["epoch"]:
            raise RuntimeError("reader replacement changed the writer authority")
        return {"lost_reader": lost_node, "before": before_readers, "after": replacement,
                "replacement_seconds": round(time.monotonic() - started, 3),
                "owner_session": after["owner"]["session"]}
    finally:
        compose(path, profiles, "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300", node_name(lost_index))


def prove_warm_promotion(path: Path, profiles: tuple[str, ...], port: int) -> dict:
    set_reader_target(port, 19, 2)
    warm = prove_readers(port, 20, 2, 19)
    sessions = node_inventory(path, profiles, 20)
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
                               {"request_id": str(uuid.uuid4()), "body": "written by the promoted warm reader"})
        if comment.get("body") != "written by the promoted warm reader":
            raise RuntimeError("promoted reader did not acknowledge a new mutation")
        return {"old_owner_session": before["owner"]["session"],
                "new_owner_session": after["owner"]["session"], "promoted_reader": successor,
                "previous_readers": warm, "root_before": before["root"], "root_after": after["root"],
                "recovery_and_write_seconds": round(time.monotonic() - started, 3)}
    finally:
        compose(path, profiles, "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300", node_name(owner_index))


def prove_all_reader_loss(path: Path, profiles: tuple[str, ...], stage: dict, port: int, project: str) -> dict:
    policy = set_reader_target(port, 20, 2)
    if policy["desired_readers"] != 2:
        raise RuntimeError("all-reader-loss target was not applied")
    before_readers = prove_readers(port, 20, 2, 20)
    sessions = node_inventory(path, profiles, 20)
    node_by_id = {node: node_name(index) for index, node in sessions.values()}
    node_by_session = {session: node_name(index) for session, (index, _) in sessions.items()}
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


def prove_authority_outage(path: Path, profiles: tuple[str, ...], port: int) -> dict:
    url = node_url(1, port) + issue_path(1) + "/1?read=replica"
    request_json("GET", node_url(1, port) + issue_path(1) + "/1")
    prove_readers(port, 20, 19)
    before = replica_issue(url, 1)
    started = time.monotonic()
    try:
        compose(path, profiles, "pause", "rustfs")
        try:
            with urllib.request.urlopen(url, timeout=7) as response:
                raise RuntimeError(f"replica returned HTTP {response.status} without authority")
        except urllib.error.HTTPError as error:
            body = json.load(error)
            if error.code != 503 or body.get("error", {}).get("code") != "replica_unavailable":
                raise RuntimeError(f"unexpected authority outage response: {error.code} {body}") from error
            unavailable_seconds = round(time.monotonic() - started, 3)
    finally:
        compose(path, profiles, "unpause", "rustfs")
    deadline = time.monotonic() + 30
    while True:
        try:
            after = replica_issue(url, 1)
            break
        except (OSError, urllib.error.HTTPError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(1)
    if after[2] != before[2] or after[1] < before[1]:
        raise RuntimeError("authority recovery changed incarnation or regressed the receipt")
    return {"http_status": 503, "error_code": "replica_unavailable",
            "unavailable_seconds": unavailable_seconds,
            "before_sequence": before[1], "after_sequence": after[1], "incarnation": after[2]}


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
        stage["read_measurement"] = measure_reads(path, profiles, args.node_port_base, size)
        stage["refresh_measurement"] = measure_refresh(
            path, profiles, args.node_port_base, size,
            set(stage["replicas"]["reader_counts"]), stage["owners"]["work-01"],
        )
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
        provider_limits = compose(path, profiles, "exec", "-T", "rustfs", "cat", "/proc/1/limits")
        nofile = next(line.split()[3:5] for line in provider_limits.splitlines() if line.startswith("Max open files"))
        if nofile != [str(RUSTFS_NOFILE_LIMIT), str(RUSTFS_NOFILE_LIMIT)]:
            raise RuntimeError(f"unexpected RustFS descriptor limits: {nofile}")
        stage["rustfs_resources"] = {
            "process_limits": provider_limits,
            "open_descriptors": len(compose(path, profiles, "exec", "-T", "rustfs", "ls", "/proc/1/fd").splitlines()),
        }
        report["stages"].append(stage)
        (path.parent / "read-replica-report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"Verified {size} nodes and {target} distinct S3-rooted issue readers", flush=True)
        previous = size
    report["reader_replacement"] = prove_reader_replacement(path, phases[-1][1], args.node_port_base)
    (path.parent / "read-replica-report.json").write_text(json.dumps(report, indent=2) + "\n")
    report["warm_promotion"] = prove_warm_promotion(path, phases[-1][1], args.node_port_base)
    (path.parent / "read-replica-report.json").write_text(json.dumps(report, indent=2) + "\n")
    report["all_reader_loss"] = prove_all_reader_loss(
        path, phases[-1][1], report["stages"][-1], args.node_port_base, args.project
    )
    (path.parent / "read-replica-report.json").write_text(json.dumps(report, indent=2) + "\n")
    report["authority_outage"] = prove_authority_outage(path, phases[-1][1], args.node_port_base)
    (path.parent / "read-replica-report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(path.parent / "read-replica-report.json")


if __name__ == "__main__":
    main()
