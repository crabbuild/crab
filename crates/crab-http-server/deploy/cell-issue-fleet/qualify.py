#!/usr/bin/env python3
"""Run the Cell issue service through 3, 5, 10, and 20 Compose nodes."""

import argparse
import json
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from contextlib import contextmanager
from pathlib import Path

sys.dont_write_bytecode = True

from render import BUCKET, CONFIG, CPU_LIMIT, MEMORY_LIMIT, ROOT, node_name, render


def command(*args: str) -> str:
    result = subprocess.run(args, check=True, text=True, stdout=subprocess.PIPE)
    return result.stdout.strip()


def compose(path: Path, profiles: tuple[str, ...], *args: str) -> str:
    flags = [flag for profile in profiles for flag in ("--profile", profile)]
    return command("docker", "compose", "--file", str(path), *flags, *args)


@contextmanager
def restart_after_fault(receipt: dict, restart):
    # Store proof before cleanup: restarting an old owner must neither erase a
    # successful takeover nor replace an acknowledged-data failure in the report.
    failed = False
    try:
        yield
    except BaseException as error:
        failed = True
        receipt["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        try:
            restart()
        except Exception as error:
            receipt["restart"] = {"passed": False, "error": f"{type(error).__name__}: {error}"}
            if not failed:
                raise
        else:
            receipt["restart"] = {"passed": True}


def image_provenance(reference: str) -> dict:
    image = json.loads(command("docker", "image", "inspect", reference))[0]
    source = (image["Config"].get("Labels") or {}).get("org.opencontainers.image.revision", "")
    if not re.fullmatch(r"[0-9a-f]{40}", source):
        raise RuntimeError("server image lacks a valid source revision label; rebuild or import a qualified image")
    return {"image": image["Id"], "source": source, "platform": f"{image['Os']}/{image['Architecture']}"}


def build_image(project: str, source: str) -> None:
    # Stream only the committed tree: ignored files and concurrent workspace
    # edits cannot silently change the source attributed to the resulting image.
    with subprocess.Popen(
        ["git", "-C", str(ROOT), "archive", "--format=tar", source], stdout=subprocess.PIPE,
    ) as archive:
        try:
            subprocess.run([
                "docker", "build", "--file", "crates/crab-http-server/deploy/Dockerfile",
                "--label", f"org.opencontainers.image.revision={source}",
                "--tag", f"{project}:local", "-",
            ], stdin=archive.stdout, check=True)
        finally:
            archive.stdout.close()
        if archive.wait() != 0:
            raise RuntimeError("could not archive the committed source for the server image")


def pin_image(path: Path, source: str) -> dict:
    deployment = json.loads(path.read_text())
    reference = deployment["services"]["node-01"]["image"]
    image = image_provenance(reference)
    if image["source"] != source:
        raise RuntimeError(f"server image source revision {image['source']} differs from checkout {source}")
    # Containerd can discard an untagged image index even while a container
    # references it. Retain this run's image if the mutable build tag moves.
    retained = f"{deployment['name']}:qualified-{image['image'].removeprefix('sha256:')}"
    command("docker", "image", "tag", image["image"], retained)
    for service in deployment["services"].values():
        if service["image"] == reference:
            service["image"] = image["image"]
            service["pull_policy"] = "never"
            service.pop("build", None)
    deployment["services"]["release-init"]["command"][-1] = image["image"]
    path.write_text(json.dumps(deployment, indent=2) + "\n")
    return image


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
            if isinstance(error, urllib.error.HTTPError):
                error.close()
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
        "{{.HostConfig.NanoCpus}} {{.HostConfig.Memory}} {{.HostConfig.MemorySwap}} {{.State.Health.Status}} {{.Image}}",
        container_id,
    ).split()
    expected_image = json.loads(path.read_text())["services"][service]["image"]
    if inspected[-1] != expected_image or not re.fullmatch(r"sha256:[0-9a-f]{64}", expected_image):
        raise RuntimeError(f"{service} is not running the pinned server image: {inspected[-1]}")
    if inspected[:-1] != [str(CPU_LIMIT), str(MEMORY_LIMIT), str(MEMORY_LIMIT), "healthy"]:
        raise RuntimeError(f"{service} has unexpected resource limits or health: {inspected}")
    capacity = json.loads(
        compose(path, profiles, "exec", "-T", service, "crab-http-server", "--config", CONFIG, "cells", "capacity", "--json", "--live")
    )
    if capacity["resources"]["memory_bytes"] != MEMORY_LIMIT or capacity["admission"]["active_cells"] < 1:
        raise RuntimeError(f"{service} did not admit the 1 GiB Cell profile")
    recorded_sessions = compose(
        path,
        profiles,
        "exec",
        "-T",
        service,
        "sh",
        "-ec",
        'for path in /var/lib/crab/cells/sessions/*; do [ -d "$path" ] && basename "$path"; done',
    )
    live_sessions = [
        session for session in recorded_sessions.splitlines()
        if json.loads(compose(
            path, profiles, "exec", "-T", service, "crab-http-server", "--config", CONFIG,
            "cells", "node", "--session", session, "--json",
        ))["live"]
    ]
    if len(live_sessions) != 1:
        raise RuntimeError(f"{service} has {len(live_sessions)} live boot sessions")
    session = live_sessions[0]
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


def run_stage(path: Path, profiles: tuple[str, ...], previous: int, size: int, gateway_port: int, node_port_base: int, cells: int) -> dict:
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

    initial_cells = range(1, cells + 1) if previous == 0 else ()
    for index in initial_cells:
        create_repository(path, profiles, index)
        issue = request_json(
            "POST",
            node_url((index - 1) % size + 1, node_port_base) + issue_path(index),
            {
                "request_id": f"00000000-0000-4000-8000-{index:012x}",
                "title": f"Cell issue {index}",
                "body": "Durable issue created through a constrained Cell node",
            },
        )
        if issue.get("number") != 1:
            raise RuntimeError(f"node {index} did not create the expected issue: {issue}")
        label = request_json(
            "POST",
            node_url((index - 1) % size + 1, node_port_base) + f"/api/repos/demo/work-{index:02d}/labels",
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
    for index in range(1, cells + 1):
        visible = request_json("GET", gateway + issue_path(index) + "/1")
        if visible.get("title") != f"Cell issue {index}":
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
        visible = request_json("GET", node_url(index, node_port_base) + issue_path(1) + "/1")
        if visible.get("title") != "Cell issue 1":
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
        "cells": cells,
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "node_capacity_active_cells": capacities,
        "owners": owners,
        "distinct_owners": len(set(owners.values())),
        "rustfs_cell_objects_present": True,
        "container_stats": {sample["Name"]: {"memory": sample["MemUsage"], "cpu": sample["CPUPerc"]} for sample in samples},
    }


def prove_owner_loss(path: Path, profiles: tuple[str, ...], owner: str, gateway_port: int, cell: int, receipt: dict) -> None:
    observer = "node-01" if owner != "node-01" else "node-02"
    status_args = ("exec", "-T", observer, "crab-http-server", "--config", CONFIG, "cells", "status", "--owner", "demo", "--name", f"work-{cell:02d}")
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
    restart = lambda: compose(path, profiles, "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300", owner)
    with restart_after_fault(receipt, restart):
        compose(path, profiles, "kill", "--signal", "SIGKILL", owner)
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            try:
                # An idle Cell is acquired by a request, so drive the public
                # read before checking whether another owner has claimed it.
                with urllib.request.urlopen(
                    f"http://127.0.0.1:{gateway_port}{issue_path(cell)}/1", timeout=5
                ) as response:
                    issues = json.load(response)
                after = json.loads(compose(path, profiles, *status_args))
                session = (after.get("owner") or {}).get("session")
                if session is None or session == before["owner"]["session"]:
                    time.sleep(1)
                    continue
                if issues.get("title") != f"Cell issue {cell}":
                    raise RuntimeError("recovered owner did not return the acknowledged issue")
                root = after.get("root") or {}
                if root.get("commit_sequence", -1) < before["root"]["commit_sequence"] or root.get("txid", -1) < before["root"]["txid"]:
                    raise RuntimeError("successor regressed the published RustFS root")
                receipt.update({
                    "lost_node": owner,
                    "old_session": before["owner"]["session"],
                    "new_session": after["owner"]["session"],
                    "root_before": before["root"],
                    "root_after": root,
                    "same_root": root == before["root"],
                    "recovery_seconds": round(time.monotonic() - started, 3),
                })
                return
            except (OSError, subprocess.CalledProcessError, KeyError, ValueError, TypeError):
                time.sleep(1)
        raise RuntimeError("owner-loss recovery did not complete in 120 seconds")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--gateway-port", type=int, default=18080)
    parser.add_argument("--node-port-base", type=int, default=18100)
    parser.add_argument("--rustfs-port", type=int, default=19010)
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--load-stages", action="store_true", help="run gateway load while each stage is active")
    parser.add_argument("--cells", type=int, default=20, help="fixed Cell count across every node stage")
    parser.add_argument("--load-rate", type=float, default=5, help="scheduled create/read pairs per second")
    parser.add_argument("--load-duration", type=float, default=60)
    parser.add_argument("--load-max-in-flight", type=int, default=64)
    args = parser.parse_args()
    # Import after module initialization: load uses the same Compose helpers.
    from load import Workload, wait_for_placement
    try:
        Workload(args.cells, args.load_rate, args.load_duration, args.load_max_in_flight, 0)
    except ValueError as error:
        parser.error(str(error))
    source = command("git", "-C", str(ROOT), "rev-parse", "HEAD")
    if command("git", "-C", str(ROOT), "status", "--porcelain"):
        raise RuntimeError("qualification requires a clean committed checkout")
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
        build_image(args.project, source)
    image = pin_image(path, source)
    compose(path, ("five", "ten", "twenty"), "config", "--quiet")
    report = {
        "project": args.project,
        **image,
        "started_at": datetime.now(timezone.utc).isoformat(),
        "node_cpu_limit": 1,
        "node_memory_limit_bytes": MEMORY_LIMIT,
        "stages": [],
        "passed": False,
    }
    phases = [(3, ()), (5, ("five",)), (10, ("five", "ten")), (20, ("five", "ten", "twenty"))]
    previous = 0
    last_load = None
    try:
        for size, profiles in phases:
            stage = run_stage(path, profiles, previous, size, args.gateway_port, args.node_port_base, args.cells)
            report["stages"].append(stage)
            (path.parent / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            stage["placement"] = {}
            owners, _ = wait_for_placement(path, profiles, size, args.cells, stage["placement"])
            stage["owners"] = {f"work-{cell:02d}": owner for cell, owner in owners.items()}
            stage["distinct_owners"] = len(set(owners.values()))
            (path.parent / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            print(f"Verified {size} nodes and {args.cells} Cell-backed issue services", flush=True)
            if args.load_stages:
                output = path.parent / f"load-{size}-stage.json"
                stage["load_report"] = output.name
                subprocess.run([
                    sys.executable,
                    str(Path(__file__).with_name("load.py")),
                    "--state", str(path.parent),
                    "--nodes", str(size),
                    "--gateway-port", str(args.gateway_port),
                    "--cells", str(args.cells),
                    "--rate", str(args.load_rate),
                    "--duration", str(args.load_duration),
                    "--max-in-flight", str(args.load_max_in_flight),
                    "--output", str(output),
                ], check=True)
                last_load = output
            previous = size
        if last_load:
            report["owner_loss"] = json.loads(last_load.read_text())["owner_loss"]
            report["owner_loss_source"] = last_load.name
        else:
            report["owner_loss"] = {}
            prove_owner_loss(path, phases[-1][1], report["stages"][-1]["owners"][f"work-{args.cells:02d}"], args.gateway_port, args.cells, report["owner_loss"])
        report["passed"] = True
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        report["error"] = str(error)
        raise
    finally:
        (path.parent / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(path.parent / "report.json")


if __name__ == "__main__":
    main()
