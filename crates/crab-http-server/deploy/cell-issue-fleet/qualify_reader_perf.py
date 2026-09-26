#!/usr/bin/env python3
"""Compare source-bound replica and owner builds on the retained five-node fixture."""

import argparse
import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path

from qualify import command, compose, issue_path, node_url, request_json
from qualify_read_replicas import cost_delta, cost_snapshot, node_inventory, prove_readers, set_reader_target
from qualify_reader_load import measure, resources
from render import node_name


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--runtime-source", required=True)
    parser.add_argument("--node-port-base", type=int, default=18500)
    parser.add_argument("--rounds", type=int, choices=(1, 2, 3), default=3)
    args = parser.parse_args()
    if args.report.exists() or command("git", "status", "--porcelain"):
        raise RuntimeError("requires committed source and a new report path")
    source = command("git", "rev-parse", args.runtime_source + "^{commit}")
    image = command("docker", "image", "inspect", "--format", "{{.Id}}", args.image)
    raw = (args.state / "reader-first-loss-report.json").read_bytes()
    previous = json.loads(raw)
    labels = json.loads(command("docker", "image", "inspect", "--format", "{{json .Config.Labels}}", image)) or {}
    recorded_source = previous["runtime_source"] if image == previous["image"] else labels.get("org.opencontainers.image.revision")
    if recorded_source != source:
        raise RuntimeError("runtime source is not bound to this image")
    config = json.loads((args.state / "retention-compose.json").read_text())
    if not previous.get("finished_at") or config["name"] != previous["project"]:
        raise RuntimeError("requires the completed disposable loss fixture")
    old_image = config["services"]["node-01"]["image"]
    for service in config["services"].values():
        if service.get("image") == old_image:
            service["image"] = image
    path = args.report.with_suffix(".compose.json")
    if path.exists():
        raise RuntimeError("preserve the previous derived Compose file")
    path.write_text(json.dumps(config, indent=2) + "\n")
    nodes = [node_name(index) for index in range(1, 6)]
    compose(path, ("five",), "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300",
            "gateway", *nodes)
    for node in nodes:
        container = compose(path, ("five",), "ps", "--quiet", node)
        if command("docker", "inspect", "--format", "{{.Image}}", container) != image:
            raise RuntimeError("running container differs from the selected image")
    set_reader_target(args.node_port_base, 1, 4)
    expected = {"title": "Cell issue on node 1", "body": previous["acknowledged_body"]}
    current = request_json("GET", node_url(1, args.node_port_base) + issue_path(1) + "/1")
    if any(current.get(key) != value for key, value in expected.items()):
        raise RuntimeError("fixture lost its acknowledged value")
    report = {"runtime_source": source, "image": image, "project": config["name"],
              "runner_source": command("git", "rev-parse", "HEAD"),
              "fixture_report_sha256": hashlib.sha256(raw).hexdigest(),
              "started_at": datetime.now(timezone.utc).isoformat(),
              "nodes": node_inventory(path, ("five",), 5),
              "ready_readers": prove_readers(args.node_port_base, 5, 4),
              "rounds": [], "scope": "single host; same retained fixture and 1 vCPU/1 GiB nodes; "
              "8 closed-loop clients through two ingresses; 60 seconds per mode; alternating order"}
    for index in range(args.rounds):
        pair = {}
        report["rounds"].append(pair)
        for mode in (("owner", "replica") if index % 2 == 0 else ("replica", "owner")):
            before = cost_snapshot(path, ("five",), 5)
            measured = measure(args.node_port_base, mode, expected)
            measured["cost"] = cost_delta(before, cost_snapshot(path, ("five",), 5))
            measured["resources_after"] = resources(path)
            pair[mode] = measured
            args.report.write_text(json.dumps(report, indent=2) + "\n")
            if any(result["errors"] for result in measured["ingress"].values()):
                raise RuntimeError("workload returned errors; report retained")
            if mode == "replica":
                if any(len(result["reader_counts"]) != 4 for result in measured["ingress"].values()):
                    raise RuntimeError("an ingress did not use all four readers")
                if measured["minimum_replica_sequence"] < previous["after_write"]["root"]["commit_sequence"]:
                    raise RuntimeError("replica returned an older receipt than the acknowledged value")
        pair["comparison"] = {
            "throughput_ratio": pair["replica"]["requests_per_second"] / pair["owner"]["requests_per_second"],
            "p50_ratio": pair["replica"]["p50_ms"] / pair["owner"]["p50_ms"],
            "p99_ratio": pair["replica"]["p99_ms"] / pair["owner"]["p99_ms"],
        }
        print(json.dumps({"round": index + 1, **pair["comparison"]}), flush=True)
    report["similar_performance"] = all(
        pair["comparison"]["throughput_ratio"] >= .8
        and pair["comparison"]["p50_ratio"] <= 1.2
        and pair["comparison"]["p99_ratio"] <= 1.2 for pair in report["rounds"]
    )
    report["finished_at"] = datetime.now(timezone.utc).isoformat()
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    if not report["similar_performance"]:
        raise SystemExit("replica performance remains outside the comparison limits; report retained")


if __name__ == "__main__":
    main()
