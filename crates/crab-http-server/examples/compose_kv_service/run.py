#!/usr/bin/env python3
"""Exercise a disposable 3 → 5 → 10 → 20 Cell service fleet on RustFS."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid


COMPOSE_FILE = Path(__file__).with_name("compose.yaml")
REPO_ROOT = COMPOSE_FILE.resolve().parents[4]
SOURCE_FILES = (
    "Cargo.lock",
    "crates/crab-http-server/Cargo.toml",
    "crates/crab-http-server/examples/compose_kv_service/main.rs",
    "crates/crab-http-server/examples/compose_kv_service/compose.yaml",
    "crates/crab-http-server/examples/compose_kv_service/Dockerfile",
    "crates/crab-http-server/examples/compose_kv_service/build-image.sh",
    "crates/crab-http-server/examples/compose_kv_service/run.py",
)
STAGES = (3, 5, 10, 20)
CPU_NANOSECONDS = 1_000_000_000
MEMORY_BYTES = 1_073_741_824


def command(arguments, *, env, output=True):
    result = subprocess.run(
        arguments,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(
            f"{' '.join(arguments[:4])} failed ({result.returncode}): "
            f"{result.stderr[-1000:]}"
        )
    return result.stdout.strip() if output else None


def compose(project, *arguments, env):
    return command(
        ["docker", "compose", "--project-name", project, "--file", str(COMPOSE_FILE), *arguments],
        env=env,
    )


def inspect(container, env):
    return json.loads(command(["docker", "inspect", container], env=env))[0]


def wait_healthy(project, service, env):
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        container = compose(project, "ps", "--quiet", service, env=env)
        if container:
            details = inspect(container, env)
            health = details["State"].get("Health", {}).get("Status")
            if health == "healthy":
                return details
            if details["State"]["Status"] == "exited" or health == "unhealthy":
                raise RuntimeError(f"{service} exited or became unhealthy")
        time.sleep(2)
    raise RuntimeError(f"{service} did not become healthy")


def request(project, node, method, path, *, body=None, env):
    arguments = [
        "exec", "--no-TTY", "node-01", "curl", "--silent", "--show-error",
        "--fail-with-body", "--max-time", "20", "--request", method,
    ]
    if body is not None:
        arguments.extend(["--header", "content-type: application/json", "--data", json.dumps(body)])
    arguments.append(f"http://{node}:8080{path}")
    deadline = time.monotonic() + 90
    while True:
        try:
            return json.loads(compose(project, *arguments, env=env))
        except (RuntimeError, json.JSONDecodeError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(2)


def object_count(project, prefix, env):
    raw = compose(
        project, "run", "--rm", "--no-deps", "--entrypoint", "aws", "bucket-init",
        "--endpoint-url", "http://rustfs:9000", "s3api", "list-objects-v2",
        "--bucket", "crab-cell-scale", "--prefix", prefix, "--output", "json", env=env,
    )
    return len(json.loads(raw).get("Contents", []))


def project_is_unused(project, env):
    for kind in ("container", "volume", "network"):
        arguments = ["docker", kind, "ls", "--quiet"]
        if kind == "container":
            arguments.append("--all")
        arguments.extend(["--filter", f"label=com.docker.compose.project={project}"])
        found = command(
            arguments, env=env,
        )
        if found:
            raise RuntimeError(f"refusing to reuse Compose project {project}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", default=f"crab-cell-scale-{uuid.uuid4().hex[:10]}")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--keep", action="store_true", help="leave this run's containers and volume up")
    args = parser.parse_args()
    if not args.project.startswith("crab-cell-scale-") or not all(
        character.isalnum() or character == "-" for character in args.project
    ):
        parser.error("project must be a unique crab-cell-scale-* name")
    target = Path(os.environ.get("CARGO_TARGET_DIR", ""))
    target_parent = Path.home() / "Workspace/crabbuild-target"
    if not target.is_absolute() or target_parent not in target.parents:
        parser.error("CARGO_TARGET_DIR must be a per-checkout directory on the mounted workspace")
    if not (Path.home() / "Workspace").is_dir():
        parser.error("Workspace volume is unavailable")
    report_path = args.report or target / "cell-scale" / f"{args.project}.json"
    for name in ("RUSTFS_ACCESS_KEY", "RUSTFS_SECRET_KEY"):
        if not os.environ.get(name):
            parser.error(f"{name} is required")
    env = os.environ.copy()
    if not env.get("DOCKER_HOST"):
        env["DOCKER_HOST"] = command(
            ["docker", "context", "inspect", "--format", "{{.Endpoints.docker.Host}}"],
            env=env,
        )
        env.pop("DOCKER_CONTEXT", None)
    env["CRAB_CELL_PREFIX"] = f"cell-scale/{args.project}"
    image = env.get("CRAB_CELL_SCALE_IMAGE", "crab-cell-scale:local")
    project_is_unused(args.project, env)
    compose(args.project, "config", "--quiet", env=env)
    image_id = command(["docker", "image", "inspect", image, "--format", "{{.Id}}"], env=env)
    source = command(["git", "rev-parse", "HEAD"], env=env)
    result = {
        "schema": "crab.cell-scale.compose.local/v1",
        "project": args.project,
        "source_revision": source,
        "source_files_sha256": {
            path: hashlib.sha256((REPO_ROOT / path).read_bytes()).hexdigest()
            for path in SOURCE_FILES
        },
        "image": image_id,
        "prefix": env["CRAB_CELL_PREFIX"],
        "provider": "docker-rustfs",
        "qualification": "local-simulation-only",
        "stages": [],
    }
    created = False
    values = {}
    try:
        for size in STAGES:
            started = time.monotonic()
            nodes = [f"node-{index:02}" for index in range(1, size + 1)]
            created = True
            compose(args.project, "up", "--detach", "--no-build", *nodes, env=env)
            resources = {}
            for node in nodes:
                details = wait_healthy(args.project, node, env)
                host = details["HostConfig"]
                if host["NanoCpus"] != CPU_NANOSECONDS or host["Memory"] != MEMORY_BYTES:
                    raise RuntimeError(f"{node} has incorrect CPU or memory limits")
                resources[node] = {
                    "container": details["Id"],
                    "cpu_nanocpus": host["NanoCpus"],
                    "memory_bytes": host["Memory"],
                }
                if node not in values:
                    value = f"{args.project}:{node}"
                    response = request(
                        args.project, node, "PUT", "/kv/scale-marker",
                        body={"request_id": str(uuid.uuid4()), "value": value}, env=env,
                    )
                    if response != {"committed": True}:
                        raise RuntimeError(f"{node} did not acknowledge its Cell write")
                    values[node] = value
            cell_status = {}
            for node in nodes:
                health = request(args.project, node, "GET", "/health", env=env)
                read = request(args.project, node, "GET", "/kv/scale-marker", env=env)
                if health.get("ready") is not True or health.get("active_cells") != 1:
                    raise RuntimeError(f"{node} does not have one ready Cell")
                if read.get("value") != values[node]:
                    raise RuntimeError(f"{node} did not retain its acknowledged value")
                cell_status[node] = health
            objects = object_count(args.project, env["CRAB_CELL_PREFIX"], env)
            if objects < size:
                raise RuntimeError("RustFS did not retain the expected Cell objects")
            raw_stats = command(
                ["docker", "stats", "--no-stream", "--format", "{{json .}}",
                 *(resources[node]["container"] for node in nodes)], env=env,
            )
            stats = [json.loads(line) for line in raw_stats.splitlines()]
            if len(stats) != size:
                raise RuntimeError("Docker did not report resource usage for every node")
            result["stages"].append({
                "nodes": size,
                "elapsed_seconds": round(time.monotonic() - started, 3),
                "objects": objects,
                "limits": resources,
                "cells": cell_status,
                "docker_stats": stats,
                "retained_values": len(values),
            })
            print(f"PASS {size} nodes, {len(values)} Cell values, {objects} RustFS objects", flush=True)
        result["status"] = "passed"
    except Exception as error:
        result["status"] = "failed"
        result["error"] = str(error)
        raise
    finally:
        report_path.parent.mkdir(parents=True, exist_ok=True)
        report_path.write_text(json.dumps(result, indent=2) + "\n")
        print(f"Report: {report_path}", flush=True)
        if created and not args.keep:
            compose(args.project, "down", "--volumes", "--remove-orphans", env=env)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.SubprocessError) as error:
        print(f"FAIL {error}", file=sys.stderr)
        sys.exit(1)
