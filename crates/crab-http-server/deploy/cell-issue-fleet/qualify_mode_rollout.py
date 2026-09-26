#!/usr/bin/env python3
"""Qualify a drained three-node fleet-to-object rollout against local RustFS."""

import argparse
import hashlib
import json
import subprocess
import uuid
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path

from qualify import command, compose, issue_path, node_url, prove_node, request_json, run_stage
from qualify_read_replicas import prove_readers, set_reader_target
from render import CONFIG, ROOT, node_name, render


def metrics(path: Path, index: int) -> str:
    return compose(path, (), "exec", "-T", node_name(index), "crab-http-server",
                   "--config", CONFIG, "cells", "metrics")


def proof_count(sample: str, source: str) -> float:
    name = f'crab_cell_durability_proofs_total{{source="{source}"}}'
    return sum(float(line.split()[-1]) for line in sample.splitlines() if line.startswith(name + " "))


def comment(port: int, body: str) -> dict:
    return request_json("POST", node_url(1, port) + issue_path(1) + "/1/comments",
                        {"request_id": str(uuid.uuid4()), "body": body})


def verify_values(port: int, bodies: dict[int, set[str]]) -> None:
    for index in range(1, 4):
        issue = request_json("GET", node_url(1, port) + issue_path(index) + "/1")
        labels = request_json("GET", node_url(1, port) + f"/api/repos/demo/work-{index:02d}/labels")
        if issue["title"] != f"Cell issue on node {index}" or labels["items"][0]["name"] != "distributed":
            raise RuntimeError("rollout changed an acknowledged issue or label")
        comments = request_json("GET", node_url(1, port) + issue_path(index) + "/1/comments?limit=50")
        if not bodies[index].issubset({item["body"] for item in comments["items"]}):
            raise RuntimeError("rollout lost an acknowledged comment")


def drain_fleet(path: Path, nodes: list[str]) -> dict:
    compose(path, (), "stop", "gateway")
    compose(path, (), "stop", *nodes)
    states = {}
    for service in nodes:
        container = compose(path, (), "ps", "--all", "--quiet", service)
        states[service] = json.loads(command("docker", "inspect", "--format", "{{json .State}}", container))
    return states


def require_drained(states: dict) -> None:
    for service, state in states.items():
        if state["Running"] or state["ExitCode"] != 0 or state["OOMKilled"]:
            raise RuntimeError(f"{service} did not finish the coverage barrier; fleet config retained")


def exercise_drain_faults(path: Path, nodes: list[str], enrolled: list[dict],
                          port: int, acknowledged: dict, report: dict) -> None:
    configs = {file.name: file.read_bytes() for file in (path.parent / "config").glob("*.toml")}
    active = next(node for node in enrolled if node["log"]["active"])
    by_node = {node["node"]: node_name(index) for index, node in enumerate(enrolled, 1)}
    followers = [by_node[node] for node in active["log"]["member_nodes"]]
    if len(followers) != 2 or len(set(followers)) != 2:
        raise RuntimeError("fault fixture must have two distinct durability followers")
    report["drain_faults"] = []
    for fault in ("all_followers_lost", "provider_unavailable"):
        if fault == "all_followers_lost":
            compose(path, (), "kill", "--signal", "SIGKILL", *followers)
        else:
            compose(path, (), "stop", "rustfs")
        states = drain_fleet(path, nodes)
        try:
            require_drained(states)
        except RuntimeError:
            pass
        else:
            raise RuntimeError(f"{fault} unexpectedly passed the rollout barrier")
        if any((path.parent / "config" / name).read_bytes() != value for name, value in configs.items()):
            raise RuntimeError("failed drain changed the deployment configuration")
        report["drain_faults"].append({"fault": fault, "states": states,
                                      "followers": followers if fault == "all_followers_lost" else [],
                                      "fleet_config_unchanged": True})
        (path.parent / "mode-rollout-report.json").write_text(json.dumps(report, indent=2) + "\n")
        compose(path, (), "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300")
        verify_values(port, acknowledged)
        report["drain_faults"][-1]["acknowledged_values_recovered"] = True


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--gateway-port", type=int, default=18080)
    parser.add_argument("--node-port-base", type=int, default=18100)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--runtime-source", help="source commit of the existing image when skipping its build")
    parser.add_argument("--exercise-drain-faults", action="store_true")
    args = parser.parse_args()
    if args.skip_build and not args.runtime_source:
        parser.error("--skip-build requires --runtime-source")
    if command("git", "-C", str(ROOT), "status", "--porcelain"):
        raise RuntimeError("qualification requires committed source")
    label = f"label=com.docker.compose.project={args.project}"
    if any(command("docker", kind, "ls", "-q", "--filter", label) for kind in ("volume", "network")) or command("docker", "ps", "-aq", "--filter", label):
        raise RuntimeError(f"Compose project {args.project} already has resources")
    path = render(args.state, args.project, args.gateway_port, args.node_port_base)
    compose(path, (), "config", "--quiet")
    if not args.skip_build:
        subprocess.run(["docker", "compose", "--file", str(path), "build", "release-init"], check=True)
    report = {
        "runtime_source": args.runtime_source or command("git", "-C", str(ROOT), "rev-parse", "HEAD"),
        "source_commit": command("git", "-C", str(ROOT), "rev-parse", "HEAD"),
        "source_diff_sha256": hashlib.sha256(command("git", "-C", str(ROOT), "diff", "--binary", "HEAD").encode()).hexdigest(),
        "image": command("docker", "image", "inspect", "--format", "{{.Id}}", f"{args.project}:local"),
        "started_at": datetime.now(timezone.utc).isoformat(),
        "profile": "local-rustfs-fleet-to-object",
        "initial_stage": run_stage(path, (), 0, 3, args.gateway_port, args.node_port_base),
    }
    nodes = [node_name(index) for index in range(1, 4)]
    acknowledged = {index: set() for index in range(1, 4)}
    # A quiet local store can win every initial proof race. Activation follows
    # follower fsync during a mutation; polling idle logs cannot exercise it.
    for attempt in range(5):
        def write_probe(offset):
            index = offset % 3 + 1
            body = f"fleet activation {attempt}:{offset}"
            result = request_json(
                "POST", node_url(index, args.node_port_base) + issue_path(index) + "/1/comments",
                {"request_id": str(uuid.uuid4()), "body": body},
            )
            if result["body"] != body:
                raise RuntimeError("fleet proof workload returned the wrong comment")
            return index, body
        with ThreadPoolExecutor(max_workers=6) as workers:
            for index, body in workers.map(write_probe, range(18)):
                acknowledged[index].add(body)
        enrolled = []
        for index in range(1, 4):
            session, _, _ = prove_node(path, (), index)
            status = json.loads(compose(path, (), "exec", "-T", node_name(index), "crab-http-server",
                                        "--config", CONFIG, "cells", "node", "--session", session, "--json"))
            enrolled.append(status["advertisement"])
        samples = {node_name(index): metrics(path, index) for index in range(1, 4)}
        if all(node and node["log"] for node in enrolled) and any(
            proof_count(sample, "fleet") > 0 for sample in samples.values()
        ):
            break
    else:
        raise RuntimeError("the initial fleet deployment never issued a fleet durability proof")
    fleet_body = "acknowledged before the fleet-to-object drain"
    if comment(args.node_port_base, fleet_body)["body"] != fleet_body:
        raise RuntimeError("fleet comment was not acknowledged")
    acknowledged[1].add(fleet_body)
    report["fleet_comments"] = {index: sorted(bodies) for index, bodies in acknowledged.items()}
    report["fleet_metrics"] = {node_name(index): metrics(path, index) for index in range(1, 4)}
    if sum(proof_count(sample, "fleet") for sample in report["fleet_metrics"].values()) == 0:
        raise RuntimeError("the initial deployment never completed a fleet durability proof")
    report["fleet_advertisements"] = enrolled
    (path.parent / "mode-rollout-report.json").write_text(json.dumps(report, indent=2) + "\n")

    if args.exercise_drain_faults:
        exercise_drain_faults(path, nodes, enrolled, args.node_port_base, acknowledged, report)

    # A successful server shutdown includes the node-log coverage and close
    # barrier. A killed/failed drain must leave every config in fleet mode.
    report["drain"] = drain_fleet(path, nodes)
    (path.parent / "mode-rollout-report.json").write_text(json.dumps(report, indent=2) + "\n")
    require_drained(report["drain"])
    path = render(args.state, args.project, args.gateway_port, args.node_port_base, True)
    compose(path, (), "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300")
    verify_values(args.node_port_base, acknowledged)
    object_body = "acknowledged after the object-proof rollout"
    if comment(args.node_port_base, object_body)["body"] != object_body:
        raise RuntimeError("object comment was not acknowledged")
    acknowledged[1].add(object_body)
    report["object_comment"] = object_body
    report["object_metrics"] = {node_name(index): metrics(path, index) for index in range(1, 4)}
    if any(proof_count(sample, "fleet") != 0 for sample in report["object_metrics"].values()):
        raise RuntimeError("object-mode deployment issued a fleet proof")
    if sum(proof_count(sample, "object") for sample in report["object_metrics"].values()) == 0:
        raise RuntimeError("object-mode deployment did not publish an object proof")
    set_reader_target(args.node_port_base, 1, 2)
    report["readers_after_rollout"] = prove_readers(args.node_port_base, 3, 2)
    (path.parent / "mode-rollout-report.json").write_text(json.dumps(report, indent=2) + "\n")

    compose(path, (), "kill", "--signal", "SIGKILL", *nodes)
    compose(path, (), "rm", "--force", *nodes)
    for service in nodes:
        volume = f"{args.project}_{service}-data"
        owner = command("docker", "volume", "inspect", "--format", '{{index .Labels "com.docker.compose.project"}}', volume)
        if owner != args.project:
            raise RuntimeError(f"refusing to remove an unowned Cell volume: {volume}")
        command("docker", "volume", "rm", volume)
    compose(path, (), "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300")
    verify_values(args.node_port_base, acknowledged)
    report["readers_after_all_disk_loss"] = prove_readers(args.node_port_base, 3, 2)
    report["all_three_local_volumes_lost"] = True
    report["completed_at"] = datetime.now(timezone.utc).isoformat()
    (path.parent / "mode-rollout-report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(path.parent / "mode-rollout-report.json")


if __name__ == "__main__":
    main()
