#!/usr/bin/env python3
"""Exercise every Cell through every load-balanced entry node."""

import argparse
import concurrent.futures
import json
import math
import re
import subprocess
import time
import urllib.error
import urllib.request
import uuid
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

from qualify import BUCKET, CONFIG, MEMORY_LIMIT, ROOT, command, compose, issue_path, node_name, prove_node


class RequestFailure(RuntimeError):
    def __init__(self, status: int, detail: str):
        super().__init__(detail)
        self.status = status


def profiles_for(nodes: int) -> tuple[str, ...]:
    return {3: (), 5: ("five",), 10: ("five", "ten"), 20: ("five", "ten", "twenty")}[nodes]


def running_nodes(project: str) -> set[str]:
    services = command(
        "docker", "ps", "--filter", f"label=com.docker.compose.project={project}",
        "--format", '{{.Label "com.docker.compose.service"}}',
    ).splitlines()
    return {service for service in services if re.fullmatch(r"node-\d{2}", service)}


def status(path: Path, profiles: tuple[str, ...], cell: int) -> dict:
    result = compose(
        path, profiles, "exec", "-T", "node-01", "crab-http-server", "--config",
        CONFIG, "cells", "status", "--owner", "demo", "--name", f"work-{cell:02d}",
    )
    value = json.loads(result)
    if value.get("state") != "serving" or not value.get("root"):
        raise RuntimeError(f"work-{cell:02d} has no serving owner or published root")
    return value


def request(gateway: str, nodes: int, method: str, path: str, payload: dict | None = None) -> dict:
    body = json.dumps(payload).encode() if payload is not None else None
    headers = {"content-type": "application/json"} if body is not None else {}
    started = time.monotonic()
    try:
        with urllib.request.urlopen(
            urllib.request.Request(gateway + path, data=body, method=method, headers=headers),
            timeout=30,
        ) as response:
            expected = 201 if method == "POST" else 200
            if response.status != expected:
                raise RuntimeError(f"{method} {path} returned {response.status}, expected {expected}")
            upstream = response.headers.get("X-Crab-Fleet-Entry", "")
            match = re.fullmatch(r"127\.0\.0\.1:(\d+)", upstream)
            entry = int(match.group(1)) - 8200 if match else 0
            if not 1 <= entry <= nodes:
                raise RuntimeError(f"{method} {path} used an unexpected entry node: {upstream!r}")
            result = json.load(response)
    except urllib.error.HTTPError as error:
        detail = error.read(512).decode(errors="replace")
        raise RequestFailure(error.code, f"{method} {path} returned {error.code}: {detail}") from error
    return {
        "entry": node_name(entry),
        "latency_ms": (time.monotonic() - started) * 1_000,
        "body": result,
    }


def load_request(gateway: str, nodes: int, method: str, path: str, payload: dict | None = None) -> dict:
    started = time.monotonic()
    failures = []
    for attempt in range(6):
        try:
            sample = request(gateway, nodes, method, path, payload)
            sample["latency_ms"] = (time.monotonic() - started) * 1_000
            sample["retries"] = len(failures)
            sample["retry_reasons"] = failures
            return sample
        except RequestFailure as error:
            if error.status not in (429, 502, 503, 504) or attempt == 5:
                raise
            failures.append(error.status)
        except OSError:
            if attempt == 5:
                raise
            failures.append("transport")
        time.sleep(0.1 * 2 ** attempt)
    raise RuntimeError("load request retry loop did not terminate")


def percentiles(samples: list[float]) -> dict:
    ordered = sorted(samples)
    result = {
        f"p{percentile}_ms": round(ordered[math.ceil(len(ordered) * percentile / 100) - 1], 3)
        for percentile in (50, 95, 99)
    }
    result["max_ms"] = round(ordered[-1], 3)
    return result


def cover_routes(gateway: str, nodes: int) -> tuple[dict, list[dict]]:
    coverage = {}
    samples = []
    expected = {node_name(index) for index in range(1, nodes + 1)}
    for cell in range(1, nodes + 1):
        seen = set()
        for _ in range(nodes * 4):
            sample = request(gateway, nodes, "GET", issue_path(cell) + "/1")
            if sample["body"].get("number") != 1:
                raise RuntimeError(f"work-{cell:02d} did not return its original issue")
            seen.add(sample["entry"])
            samples.append({"cell": cell, "entry": sample["entry"], "latency_ms": sample["latency_ms"]})
            if seen == expected:
                break
        if seen != expected:
            raise RuntimeError(f"work-{cell:02d} missed entry nodes: {sorted(expected - seen)}")
        coverage[f"work-{cell:02d}"] = sorted(seen)
    return coverage, samples


def load_cell(gateway: str, nodes: int, cell: int, pairs: int, run_id: str) -> tuple[list[dict], dict]:
    samples = []
    latest = {}
    for pair in range(pairs):
        title = f"fleet-load-{run_id}-{cell:02d}-{pair:03d}"
        created = load_request(
            gateway, nodes, "POST", issue_path(cell),
            {
                "request_id": str(uuid.uuid5(uuid.NAMESPACE_URL, title)),
                "title": title,
                "body": "Load-balanced durable Cell issue",
            },
        )
        number = created["body"].get("number")
        if not isinstance(number, int) or created["body"].get("title") != title:
            raise RuntimeError(f"work-{cell:02d} returned an unexpected issue creation result")
        observed = load_request(gateway, nodes, "GET", issue_path(cell) + f"/{number}")
        if observed["body"].get("title") != title:
            raise RuntimeError(f"work-{cell:02d} did not read back issue {number}")
        for operation, sample in (("write", created), ("read", observed)):
            samples.append({
                "cell": cell, "operation": operation, "entry": sample["entry"],
                "latency_ms": sample["latency_ms"], "retries": sample["retries"],
                "retry_reasons": sample["retry_reasons"],
            })
        latest = {"number": number, "title": title}
    return samples, latest


def owner_map(path: Path, profiles: tuple[str, ...], nodes: int) -> tuple[dict, dict]:
    sessions = {}
    for index in range(1, nodes + 1):
        session, _, _ = prove_node(path, profiles, index)
        sessions[session] = node_name(index)
    statuses = {cell: status(path, profiles, cell) for cell in range(1, nodes + 1)}
    owners = {}
    for cell, value in statuses.items():
        session = value.get("owner", {}).get("session")
        if session not in sessions:
            raise RuntimeError(f"work-{cell:02d} has no live Compose owner")
        owners[cell] = sessions[session]
    return owners, statuses


def verify_roots(path: Path, profiles: tuple[str, ...], before: dict, nodes: int) -> dict:
    after = {}
    for cell in range(1, nodes + 1):
        baseline = before[cell]["root"]["commit_sequence"]
        deadline = time.monotonic() + 60
        while True:
            observed = status(path, profiles, cell)
            if observed["root"]["commit_sequence"] > baseline:
                after[cell] = observed
                break
            if time.monotonic() >= deadline:
                raise RuntimeError(f"work-{cell:02d} did not publish a newer RustFS root")
            time.sleep(0.5)
    return after


def recover_owner(
    path: Path, profiles: tuple[str, ...], gateway: str, nodes: int,
    owner: str, before: dict, latest: dict,
) -> dict:
    observer = "node-01" if owner != "node-01" else "node-02"
    target = nodes
    for _ in range(60):
        metrics = compose(path, profiles, "exec", "-T", owner, "crab-http-server", "--config", CONFIG, "cells", "metrics")
        uncovered = next(
            (line.split()[-1] for line in metrics.splitlines()
             if line.startswith("crab_cell_node_log_uncovered_bytes ")),
            None,
        )
        if uncovered == "0":
            break
        time.sleep(1)
    else:
        raise RuntimeError("owner did not publish its retained bytes to RustFS")
    started = time.monotonic()
    try:
        compose(path, profiles, "kill", "--signal", "SIGKILL", owner)
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            try:
                observed = request(gateway, nodes, "GET", issue_path(target) + f"/{latest['number']}")
                current = json.loads(compose(
                    path, profiles, "exec", "-T", observer, "crab-http-server", "--config",
                    CONFIG, "cells", "status", "--owner", "demo", "--name", f"work-{target:02d}",
                ))
            except (OSError, RuntimeError, subprocess.CalledProcessError, ValueError):
                time.sleep(1)
                continue
            old_session = before["owner"]["session"]
            new_session = (current.get("owner") or {}).get("session")
            root = current.get("root") or {}
            if new_session == old_session or not new_session:
                time.sleep(1)
                continue
            if observed["body"].get("title") != latest["title"]:
                raise RuntimeError("recovered owner lost an acknowledged issue")
            if (root.get("commit_sequence", -1) < before["root"]["commit_sequence"]
                    or root.get("txid", -1) < before["root"]["txid"]):
                raise RuntimeError("recovered owner regressed the published RustFS root")
            return {
                "lost_node": owner,
                "new_session": new_session,
                "recovery_seconds": round(time.monotonic() - started, 3),
                "same_root": root == before["root"],
                "entry_node": observed["entry"],
            }
        raise RuntimeError("load-balanced owner recovery did not complete in 120 seconds")
    finally:
        compose(path, profiles, "up", "--detach", "--no-build", "--wait", owner)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--nodes", type=int, choices=(3, 5, 10, 20), required=True)
    parser.add_argument("--gateway-port", type=int, default=18080)
    parser.add_argument("--pairs-per-cell", type=int, default=10)
    parser.add_argument("--output", type=Path, help="write the report to this path instead of a generated name")
    args = parser.parse_args()
    if not 1 <= args.pairs_per_cell <= 100:
        parser.error("--pairs-per-cell must be between 1 and 100")
    path = args.state.expanduser().resolve() / "compose.yaml"
    deployment = json.loads(path.read_text())
    project = deployment["name"]
    expected_nodes = {node_name(index) for index in range(1, args.nodes + 1)}
    active_nodes = running_nodes(project)
    if active_nodes != expected_nodes:
        raise RuntimeError(f"expected exactly {args.nodes} running Cell nodes, found {sorted(active_nodes)}")
    profiles = profiles_for(args.nodes)
    gateway = f"http://127.0.0.1:{args.gateway_port}"
    owners, before = owner_map(path, profiles, args.nodes)
    coverage, coverage_samples = cover_routes(gateway, args.nodes)
    run_id = uuid.uuid4().hex[:12]
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.nodes) as executor:
        futures = [
            executor.submit(load_cell, gateway, args.nodes, cell, args.pairs_per_cell, run_id)
            for cell in range(1, args.nodes + 1)
        ]
        results = [future.result() for future in futures]
    elapsed = time.monotonic() - started
    samples = [sample for cell_samples, _ in results for sample in cell_samples]
    latest = {cell: result[1] for cell, result in enumerate(results, 1)}
    entries = Counter(sample["entry"] for sample in samples)
    expected = len(samples) / args.nodes
    active_nodes = {node_name(index) for index in range(1, args.nodes + 1)}
    if set(entries) != active_nodes or min(entries.values()) < expected * 0.7 or max(entries.values()) > expected * 1.3:
        raise RuntimeError(f"load balancer did not distribute traffic evenly: {dict(entries)}")
    for cell, issue in latest.items():
        observed = request(gateway, args.nodes, "GET", issue_path(cell) + f"/{issue['number']}")
        if observed["body"].get("title") != issue["title"]:
            raise RuntimeError(f"work-{cell:02d} lost its last acknowledged issue")
    after = verify_roots(path, profiles, before, args.nodes)
    recovery = recover_owner(
        path, profiles, gateway, args.nodes, owners[args.nodes], after[args.nodes], latest[args.nodes]
    )
    report = {
        "source": command("git", "-C", str(ROOT), "rev-parse", "HEAD"),
        "source_dirty": bool(command("git", "-C", str(ROOT), "status", "--porcelain")),
        "project": project,
        "server_image": command("docker", "image", "inspect", "--format", "{{.Id}}", deployment["services"]["node-01"]["image"]),
        "rustfs": {
            "image": deployment["services"]["rustfs"]["image"],
            "image_id": command("docker", "image", "inspect", "--format", "{{.Id}}", deployment["services"]["rustfs"]["image"]),
            "bucket": BUCKET,
            "endpoint": "http://rustfs:9000",
        },
        "compose_profiles": list(profiles),
        "node_cpu_limit": deployment["services"]["node-01"]["cpus"],
        "node_memory_limit_bytes": MEMORY_LIMIT,
        "started_at": datetime.now(timezone.utc).isoformat(),
        "nodes": args.nodes,
        "cells": args.nodes,
        "pairs_per_cell": args.pairs_per_cell,
        "coverage_requests": len(coverage_samples),
        "node_cell_coverage": coverage,
        "load_requests": len(samples),
        "load_elapsed_seconds": round(elapsed, 3),
        "requests_per_second": round(len(samples) / elapsed, 2),
        "entry_requests": dict(sorted(entries.items())),
        "cell_requests": {f"work-{cell:02d}": args.pairs_per_cell * 2 for cell in range(1, args.nodes + 1)},
        "forwarded_requests": sum(sample["entry"] != owners[sample["cell"]] for sample in samples),
        "retried_requests": sum(sample["retries"] > 0 for sample in samples),
        "total_retries": sum(sample["retries"] for sample in samples),
        "retry_reasons": dict(sorted(Counter(
            str(reason) for sample in samples for reason in sample["retry_reasons"]
        ).items())),
        "latency": {
            operation: percentiles([sample["latency_ms"] for sample in samples if sample["operation"] == operation])
            for operation in ("write", "read")
        },
        "coverage_read_latency": percentiles([sample["latency_ms"] for sample in coverage_samples]),
        "roots_advanced": all(after[cell]["root"]["commit_sequence"] > before[cell]["root"]["commit_sequence"] for cell in before),
        "owner_loss": recovery,
    }
    output = args.output.expanduser().resolve() if args.output else path.parent / f"load-{args.nodes}-{run_id}.json"
    if output.exists():
        raise RuntimeError(f"load report already exists: {output}")
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(output)


if __name__ == "__main__":
    main()
