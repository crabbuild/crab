#!/usr/bin/env python3
"""Run the Cell issue service through 3, 5, 10, and 20 Compose nodes."""

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

sys.dont_write_bytecode = True

from render import BUCKET, CONFIG, CPU_LIMIT, MEMORY_LIMIT, ROOT, node_name, render


def command(*args: str) -> str:
    result = subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE)
    return result.stdout.strip()


def compose(path: Path, profiles: tuple[str, ...], *args: str) -> str:
    flags = [flag for profile in profiles for flag in ("--profile", profile)]
    return command("docker", "compose", "--file", str(path), *flags, *args)


def request_json(method: str, url: str, payload: dict | None = None) -> dict:
    body = json.dumps(payload).encode() if payload is not None else None
    for attempt in range(90):
        request = urllib.request.Request(
            url,
            data=body,
            method=method,
            headers={"content-type": "application/json"} if body is not None else {},
        )
        try:
            with urllib.request.urlopen(request, timeout=15) as response:
                return json.load(response)
        except (OSError, urllib.error.HTTPError, ValueError) as error:
            if attempt == 89:
                raise RuntimeError(f"{method} {url} failed after 90 attempts") from error
            time.sleep(1)
    raise RuntimeError("request loop did not terminate")


def node_url(index: int, port_base: int) -> str:
    return f"http://127.0.0.1:{port_base + index}"


def issue_path(index: int) -> str:
    return f"/api/repos/demo/work-{index:02d}/issues"


def create_repository(path: Path, profiles: tuple[str, ...], index: int) -> None:
    compose(
        path,
        profiles,
        "run",
        "--rm",
        "--no-deps",
        "repository-init",
        "--config",
        CONFIG,
        "repository",
        "create",
        "--owner",
        "demo",
        "--name",
        f"work-{index:02d}",
        "--prefix",
        f"demo/work-{index:02d}",
        "--description",
        "Cell issue fleet example",
    )


def prove_node(path: Path, profiles: tuple[str, ...], index: int) -> tuple[str, dict, str]:
    service = node_name(index)
    container_id = compose(path, profiles, "ps", "--quiet", service)
    if not container_id or "\n" in container_id:
        raise RuntimeError(f"{service} is not a single running container")
    inspected = command(
        "docker",
        "inspect",
        "--format",
        "{{.HostConfig.NanoCpus}} {{.HostConfig.Memory}} {{.HostConfig.MemorySwap}} {{.State.Health.Status}}",
        container_id,
    ).split()
    if inspected != [str(CPU_LIMIT), str(MEMORY_LIMIT), str(MEMORY_LIMIT), "healthy"]:
        raise RuntimeError(f"{service} has unexpected resource limits or health: {inspected}")
    capacity = json.loads(
        compose(path, profiles, "exec", "-T", service, "crab-http-server", "--config", CONFIG, "cells", "capacity", "--json", "--live")
    )
    if capacity["resources"]["memory_bytes"] != MEMORY_LIMIT or capacity["admission"]["active_cells"] < 1:
        raise RuntimeError(f"{service} did not admit the 1 GiB Cell profile")
    session = compose(
        path,
        profiles,
        "exec",
        "-T",
        service,
        "sh",
        "-ec",
        'for path in /var/lib/crab/cells/sessions/*; do [ -d "$path" ] && { basename "$path"; exit; }; done; exit 1',
    )
    return session, capacity, container_id


def object_count(path: Path, profiles: tuple[str, ...]) -> int:
    result = json.loads(
        compose(
            path,
            profiles,
            "run",
            "--rm",
            "--no-deps",
            "--entrypoint",
            "aws",
            "bucket-init",
            "--endpoint-url",
            "http://rustfs:9000",
            "s3api",
            "list-objects-v2",
            "--bucket",
            BUCKET,
            "--prefix",
            "repositories/cells/v1/",
            "--max-keys",
            "1",
        )
    )
    return result["KeyCount"]


def run_stage(path: Path, profiles: tuple[str, ...], previous: int, size: int, gateway_port: int, node_port_base: int) -> dict:
    print(f"Starting {size} Cell nodes", flush=True)
    started = time.monotonic()
    compose(path, profiles, "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300")
    gateway = f"http://127.0.0.1:{gateway_port}"
    request_json("GET", gateway + "/livez")
    sessions = {}
    capacities = {}
    containers = []
    for index in range(1, size + 1):
        session, capacity, container_id = prove_node(path, profiles, index)
        sessions[session] = node_name(index)
        capacities[node_name(index)] = capacity["admission"]["active_cells"]
        containers.append(container_id)
    if len(sessions) != size:
        raise RuntimeError("nodes did not publish distinct boot sessions")

    for index in range(previous + 1, size + 1):
        create_repository(path, profiles, index)
        issue = request_json(
            "POST",
            node_url(index, node_port_base) + issue_path(index),
            {
                "request_id": f"00000000-0000-4000-8000-{index:012x}",
                "title": f"Cell issue on node {index}",
                "body": "Durable issue created through a constrained Cell node",
            },
        )
        if issue.get("number") != 1:
            raise RuntimeError(f"node {index} did not create the expected issue: {issue}")
        label = request_json(
            "POST",
            node_url(index, node_port_base) + f"/api/repos/demo/work-{index:02d}/labels",
            {
                "request_id": f"00000000-0000-4000-9000-{index:012x}",
                "name": "distributed",
                "color": "2563eb",
                "description": "Durable Cell-backed label",
            },
        )
        if label.get("name") != "distributed":
            raise RuntimeError(f"node {index} did not create its label: {label}")

    owners = {}
    for index in range(1, size + 1):
        path_for_repo = issue_path(index) + "?state=all"
        visible = request_json("GET", gateway + path_for_repo)
        if len(visible.get("items", [])) != 1 or visible["items"][0]["title"] != f"Cell issue on node {index}":
            raise RuntimeError(f"gateway did not read Cell {index} after stage {size}")
        labels = request_json("GET", gateway + f"/api/repos/demo/work-{index:02d}/labels")
        if len(labels.get("items", [])) != 1 or labels["items"][0]["name"] != "distributed":
            raise RuntimeError(f"gateway did not read Cell {index}'s label after stage {size}")
        status = json.loads(
            compose(path, profiles, "exec", "-T", "node-01", "crab-http-server", "--config", CONFIG, "cells", "status", "--owner", "demo", "--name", f"work-{index:02d}")
        )
        owner = status.get("owner", {}).get("session")
        if status.get("state") != "serving" or owner not in sessions or not status.get("root"):
            raise RuntimeError(f"Cell {index} is not served by a live node: {status}")
        owners[f"work-{index:02d}"] = sessions[owner]
    for index in range(previous + 1, size + 1):
        visible = request_json("GET", node_url(index, node_port_base) + issue_path(1) + "?state=all")
        if len(visible.get("items", [])) != 1:
            raise RuntimeError(f"new node {index} could not route to the original Cell")
    stored_objects = object_count(path, profiles)
    if stored_objects < 1:
        raise RuntimeError("RustFS did not contain a durable Cell object")
    samples = [
        json.loads(line)
        for line in command("docker", "stats", "--no-stream", "--format", "{{json .}}", *containers).splitlines()
    ]
    return {
        "nodes": size,
        "cells": size,
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "node_capacity_active_cells": capacities,
        "owners": owners,
        "distinct_owners": len(set(owners.values())),
        "rustfs_cell_objects_present": True,
        "container_stats": {sample["Name"]: {"memory": sample["MemUsage"], "cpu": sample["CPUPerc"]} for sample in samples},
    }


def prove_owner_loss(path: Path, profiles: tuple[str, ...], owner: str, gateway_port: int) -> dict:
    observer = "node-01" if owner != "node-01" else "node-02"
    status_args = ("exec", "-T", observer, "crab-http-server", "--config", CONFIG, "cells", "status", "--owner", "demo", "--name", "work-20")
    for _ in range(60):
        metrics = compose(path, profiles, "exec", "-T", owner, "crab-http-server", "--config", CONFIG, "cells", "metrics")
        uncovered = next((line.split()[-1] for line in metrics.splitlines() if line.startswith("crab_cell_node_log_uncovered_bytes ")), None)
        if uncovered == "0":
            break
        time.sleep(1)
    else:
        raise RuntimeError("owner did not publish its retained Cell bytes to RustFS")
    before = json.loads(compose(path, profiles, *status_args))
    if before.get("state") != "serving" or not before.get("root"):
        raise RuntimeError("owner-loss Cell was not serving before SIGKILL")
    print(f"Killing {owner} and checking durable recovery", flush=True)
    started = time.monotonic()
    try:
        compose(path, profiles, "kill", "--signal", "SIGKILL", owner)
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            try:
                # An idle Cell is acquired by a request, so drive the public
                # read before checking whether another owner has claimed it.
                with urllib.request.urlopen(
                    f"http://127.0.0.1:{gateway_port}{issue_path(20)}?state=all", timeout=5
                ) as response:
                    issues = json.load(response)
                after = json.loads(compose(path, profiles, *status_args))
                session = (after.get("owner") or {}).get("session")
                if session is None or session == before["owner"]["session"]:
                    time.sleep(1)
                    continue
                if len(issues.get("items", [])) != 1 or issues["items"][0]["title"] != "Cell issue on node 20":
                    raise RuntimeError("recovered owner did not return the acknowledged issue")
                root = after.get("root") or {}
                if root.get("commit_sequence", -1) < before["root"]["commit_sequence"] or root.get("txid", -1) < before["root"]["txid"]:
                    raise RuntimeError("successor regressed the published RustFS root")
                return {
                    "lost_node": owner,
                    "old_session": before["owner"]["session"],
                    "new_session": after["owner"]["session"],
                    "root_before": before["root"],
                    "root_after": root,
                    "same_root": root == before["root"],
                    "recovery_seconds": round(time.monotonic() - started, 3),
                }
            except (OSError, subprocess.CalledProcessError, KeyError, ValueError, TypeError):
                time.sleep(1)
        raise RuntimeError("owner-loss recovery did not complete in 120 seconds")
    finally:
        compose(path, profiles, "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300", owner)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--gateway-port", type=int, default=18080)
    parser.add_argument("--node-port-base", type=int, default=18100)
    parser.add_argument("--rustfs-port", type=int, default=19010)
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()
    command("docker", "info", "--format", "{{.ServerVersion}}")
    label = f"label=com.docker.compose.project={args.project}"
    existing = [
        command("docker", "ps", "-aq", "--filter", label),
        command("docker", "volume", "ls", "-q", "--filter", label),
        command("docker", "network", "ls", "-q", "--filter", label),
    ]
    if any(existing):
        raise RuntimeError(f"Compose project {args.project} already has resources; use a fresh project name")
    path = render(args.state, args.project, args.gateway_port, args.node_port_base, args.rustfs_port)
    compose(path, (), "config", "--quiet")
    if not args.skip_build:
        subprocess.run(["docker", "compose", "--file", str(path), "build", "release-init"], check=True)
    report = {
        "project": args.project,
        "source": command("git", "-C", str(ROOT), "rev-parse", "HEAD"),
        "image": command("docker", "image", "inspect", "--format", "{{.Id}}", f"{args.project}:local"),
        "started_at": datetime.now(timezone.utc).isoformat(),
        "node_cpu_limit": 1,
        "node_memory_limit_bytes": MEMORY_LIMIT,
        "stages": [],
    }
    phases = [(3, ()), (5, ("five",)), (10, ("five", "ten")), (20, ("five", "ten", "twenty"))]
    previous = 0
    for size, profiles in phases:
        report["stages"].append(run_stage(path, profiles, previous, size, args.gateway_port, args.node_port_base))
        (path.parent / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"Verified {size} nodes and {size} Cell-backed issue services", flush=True)
        previous = size
    report["owner_loss"] = prove_owner_loss(path, phases[-1][1], report["stages"][-1]["owners"]["work-20"], args.gateway_port)
    (path.parent / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(path.parent / "report.json")


if __name__ == "__main__":
    main()
