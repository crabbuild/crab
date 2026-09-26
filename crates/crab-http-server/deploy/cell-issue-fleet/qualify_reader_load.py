#!/usr/bin/env python3
"""Measure owner/replica reads with eight clients and an identical ingress request mix."""

import argparse
import hashlib
import json
import time
import urllib.error
import urllib.request
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from itertools import cycle
from pathlib import Path
from threading import Lock

from qualify import command, compose, issue_path, node_url, request_json
from qualify_read_replicas import cost_delta, cost_snapshot, node_inventory, prove_readers, set_reader_target
from render import node_name


def resources(path: Path) -> dict:
    def sample(index):
        service = node_name(index)
        return service, {
            "process_status": compose(path, ("five",), "exec", "-T", service, "cat", "/proc/1/status"),
            "descriptors": len(compose(path, ("five",), "exec", "-T", service, "ls", "/proc/1/fd").splitlines()),
            "local_cell_disk_kib": int(compose(path, ("five",), "exec", "-T", service,
                                              "du", "-sk", "/var/lib/crab/cells").split()[0]),
        }
    with ThreadPoolExecutor(max_workers=5) as workers:
        return dict(workers.map(sample, range(1, 6)))


def measure(port: int, mode: str, expected: dict) -> dict:
    start = time.monotonic()
    deadline = start + 60
    destinations = cycle((1, 1, 1, 5))
    dispatch = Lock()

    def worker(_):
        samples = {ingress: ([], Counter(), Counter(), []) for ingress in (1, 5)}
        while time.monotonic() < deadline:
            # Fix request proportions, rather than client placement: a faster
            # ingress must not silently dominate one mode's latency samples.
            with dispatch:
                ingress = next(destinations)
            latencies, readers, errors, sequences = samples[ingress]
            url = node_url(ingress, port) + issue_path(1) + "/1"
            if mode == "replica":
                url += "?read=replica"
            requested = time.monotonic()
            try:
                with urllib.request.urlopen(url, timeout=10) as response:
                    body = json.load(response)
                    if any(body.get(key) != value for key, value in expected.items()):
                        raise RuntimeError("load query returned a different acknowledged value")
                    if mode == "replica":
                        reader = response.headers.get("x-crab-cell-reader")
                        sequence = int(response.headers.get("x-crab-cell-sequence", "-1"))
                        if not reader or len(reader) != 32 or sequence < 1:
                            raise RuntimeError("replica response omitted its observed receipt")
                        readers[reader] += 1
                        sequences.append(sequence)
                    latencies.append((time.monotonic() - requested) * 1000)
            except urllib.error.HTTPError as error:
                errors[f"http_{error.code}"] += 1
                error.close()
            except OSError as error:
                errors[type(error).__name__] += 1
        return [(ingress, *values) for ingress, values in samples.items()]

    with ThreadPoolExecutor(max_workers=8) as workers:
        samples = [sample for batch in workers.map(worker, range(8)) for sample in batch]
    elapsed = time.monotonic() - start
    latencies = sorted(value for _, values, _, _, _ in samples for value in values)
    ingress_results = {}
    for ingress in (1, 5):
        readers, errors = Counter(), Counter()
        ingress_latencies = []
        for node, values, counts, failures, _ in samples:
            if node == ingress:
                ingress_latencies.extend(values)
                readers.update(counts)
                errors.update(failures)
        ingress_latencies.sort()
        count = len(ingress_latencies)
        ingress_results[node_name(ingress)] = {
            "successful_reads": count, "requests_per_second": count / elapsed,
            "requests_started": count + sum(errors.values()),
            "p50_ms": ingress_latencies[int((count - 1) * .50)] if count else None,
            "p99_ms": ingress_latencies[int((count - 1) * .99)] if count else None,
            "reader_counts": dict(readers), "errors": dict(errors),
        }
    sequences = [value for _, _, _, _, values in samples for value in values]
    if not latencies:
        raise RuntimeError("load run completed no successful queries")
    return {"mode": mode, "offered_seconds": 60, "elapsed_seconds": elapsed,
            "concurrency": 8, "ingress_request_schedule": ["node-01"] * 3 + ["node-05"],
            "successful_reads": len(latencies), "requests_per_second": len(latencies) / elapsed,
            "p50_ms": latencies[int((len(latencies) - 1) * .50)],
            "p99_ms": latencies[int((len(latencies) - 1) * .99)],
            "ingress": ingress_results,
            "minimum_replica_sequence": min(sequences) if sequences else None}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--node-port-base", type=int, default=18100)
    args = parser.parse_args()
    raw = (args.state / "reader-first-loss-report.json").read_bytes()
    previous = json.loads(raw)
    path = args.state / "retention-compose.json"
    config = json.loads(path.read_text())
    if (not previous.get("finished_at") or config["name"] != previous["project"]
            or command("git", "status", "--porcelain")):
        raise RuntimeError("requires a completed disposable loss fixture and committed source")
    output = args.state / "reader-load-report.json"
    if output.exists():
        raise RuntimeError("preserve the existing load report before another run")
    for index in range(1, 6):
        image = command("docker", "image", "inspect", "--format", "{{.Id}}",
                        config["services"][node_name(index)]["image"])
        if image != previous["image"]:
            raise RuntimeError("load image differs from the recorded runtime")
    nodes = [node_name(index) for index in range(1, 6)]
    compose(path, ("five",), "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300",
            "gateway", *nodes)
    set_reader_target(args.node_port_base, 1, 4)
    report = {"runtime_source": previous["runtime_source"], "image": previous["image"],
              "runner_source": command("git", "rev-parse", "HEAD"), "project": config["name"],
              "previous_report_sha256": hashlib.sha256(raw).hexdigest(),
              "started_at": datetime.now(timezone.utc).isoformat(),
              "nodes": node_inventory(path, ("five",), 5),
              "ready_readers": prove_readers(args.node_port_base, 5, 4), "measurements": [],
              "scope": "one host; 60 seconds per mode; closed-loop urllib clients; "
                       "VmHWM is process-lifetime high water, other resources are boundary samples"}
    expected = {"title": "Cell issue on node 1", "body": previous["acknowledged_body"]}
    issue = request_json("GET", node_url(1, args.node_port_base) + issue_path(1) + "/1")
    if any(issue[key] != value for key, value in expected.items()):
        raise RuntimeError("fixture no longer contains the acknowledged loss-test value")
    for mode in ("owner", "replica"):
        before_resources = resources(path)
        before = cost_snapshot(path, ("five",), 5)
        measured = measure(args.node_port_base, mode, expected)
        measured["cost"] = cost_delta(before, cost_snapshot(path, ("five",), 5))
        measured["resources_before"] = before_resources
        measured["resources_after"] = resources(path)
        report["measurements"].append(measured)
        output.write_text(json.dumps(report, indent=2) + "\n")
        if any(result["errors"] for result in measured["ingress"].values()):
            raise RuntimeError(f"{mode} workload encountered errors; report retained")
        if mode == "replica" and any(len(result["reader_counts"]) != 4 for result in measured["ingress"].values()):
            raise RuntimeError("an ingress did not distribute reads to all four readers")
        if mode == "replica" and measured["minimum_replica_sequence"] < previous["after_write"]["root"]["commit_sequence"]:
            raise RuntimeError("load reader returned a receipt older than the acknowledged mutation")
    report["finished_at"] = datetime.now(timezone.utc).isoformat()
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(output)


if __name__ == "__main__":
    main()
