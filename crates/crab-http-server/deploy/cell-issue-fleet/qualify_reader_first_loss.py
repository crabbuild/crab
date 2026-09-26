#!/usr/bin/env python3
"""Lose both readers, acknowledge a write, then lose the last writer's disk."""

import argparse
import hashlib
import json
import time
import tomllib
import uuid
from datetime import datetime, timezone
from pathlib import Path

from qualify import command, compose, issue_path, node_url, request_json
from qualify_read_replicas import node_inventory, prove_readers, set_reader_target
from qualify_reader_partition import unavailable
from render import CONFIG, node_name


def remove_disks(path: Path, project: str, services: list[str]) -> list[str]:
    compose(path, (), "rm", "--force", *services)
    volumes = [f"{project}_{service}-data" for service in services]
    for volume in volumes:
        owner = command("docker", "volume", "inspect", "--format",
                        '{{index .Labels "com.docker.compose.project"}}', volume)
        if owner != project:
            raise RuntimeError(f"refusing to delete an unowned Cell volume: {volume}")
        command("docker", "volume", "rm", volume)
    return volumes


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--node-port-base", type=int, default=18100)
    args = parser.parse_args()
    raw = (args.state / "reader-retention-report.json").read_bytes()
    previous = json.loads(raw)
    path = args.state / "retention-compose.json"
    config = json.loads(path.read_text())
    if not previous.get("finished_at") or config["name"] != previous["project"]:
        raise RuntimeError("requires a completed disposable reader-retention fixture")
    if command("git", "status", "--porcelain"):
        raise RuntimeError("commit the runner before collecting evidence")
    output = args.state / "reader-first-loss-report.json"
    if output.exists():
        raise RuntimeError("preserve the existing loss-order report before another run")
    for index in range(1, 21):
        if compose(path, ("five", "ten", "twenty"), "ps", "--quiet",
                   "--status", "running", node_name(index)):
            raise RuntimeError("stop this fixture's application nodes before qualification")
    nodes = [node_name(index) for index in range(1, 4)]
    for service in nodes:
        image = command("docker", "image", "inspect", "--format", "{{.Id}}",
                        config["services"][service]["image"])
        with (args.state / "config" / f"{service}.toml").open("rb") as stream:
            mode = tomllib.load(stream)["cells"]["durability"]
        if image != previous["image"] or mode != "object":
            raise RuntimeError("fixture image or object durability differs from its receipt")
    report = {"runtime_source": previous["runtime_source"], "image": previous["image"],
              "runner_source": command("git", "rev-parse", "HEAD"), "project": config["name"],
              "previous_report_sha256": hashlib.sha256(raw).hexdigest(),
              "started_at": datetime.now(timezone.utc).isoformat()}

    def save():
        output.write_text(json.dumps(report, indent=2) + "\n")

    def control(index):
        return json.loads(compose(path, (), "exec", "-T", node_name(index), "crab-http-server",
                                  "--config", CONFIG, "cells", "status", "--owner", "demo", "--name", "work-01"))

    compose(path, (), "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300",
            "gateway", *nodes)
    set_reader_target(args.node_port_base, 1, 2)
    report["readers_before"] = prove_readers(args.node_port_base, 3, 2)
    sessions = node_inventory(path, (), 3)
    before = control(1)
    writer = sessions[before["owner"]["session"]][0]
    readers = [node_name(index) for index, node in sessions.values()
               if node in report["readers_before"]["reader_counts"]]
    if len(readers) != 2 or node_name(writer) in readers:
        raise RuntimeError("expected two distinct non-writer readers")
    report.update(before=before, writer=node_name(writer), lost_readers=readers)
    save()
    started = time.monotonic()
    compose(path, (), "kill", "--signal", "SIGKILL", *readers)
    report["reader_volumes_deleted"] = remove_disks(path, config["name"], readers)
    url = node_url(writer, args.node_port_base) + issue_path(1) + "/1"
    report["reader_unavailable"] = unavailable(url)
    issue = request_json("GET", url)
    body = "Acknowledged with no surviving read secondary " + uuid.uuid4().hex
    written = request_json("PATCH", url, {"version": issue["version"], "body": body})
    after_write = control(writer)
    if (written["body"] != body or after_write["owner"] != before["owner"]
            or after_write["epoch"] != before["epoch"]
            or after_write["root"]["commit_sequence"] <= before["root"]["commit_sequence"]):
        raise RuntimeError("reader loss prevented a same-owner S3-rooted acknowledgement")
    report.update(acknowledged_body=body, after_write=after_write,
                  write_without_readers_seconds=round(time.monotonic() - started, 3))
    save()
    compose(path, (), "kill", "--signal", "SIGKILL", node_name(writer))
    report["writer_volume_deleted"] = remove_disks(path, config["name"], [node_name(writer)])
    save()
    started = time.monotonic()
    compose(path, (), "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300",
            "gateway", *nodes)
    recovered = request_json("GET", node_url(1, args.node_port_base) + issue_path(1) + "/1")
    after = control(1)
    if (recovered["body"] != body or after["incarnation"] != before["incarnation"]
            or after["owner"]["session"] in sessions or after["epoch"] <= before["epoch"]
            or after["root"]["commit_sequence"] < after_write["root"]["commit_sequence"]):
        raise RuntimeError("fresh nodes failed to recover the write made after all readers died")
    report["sessions_after"] = node_inventory(path, (), 3)
    report["readers_after"] = prove_readers(args.node_port_base, 3, 2)
    if report["readers_after"]["min_sequence"] < after_write["root"]["commit_sequence"]:
        raise RuntimeError("replacement reader predates the acknowledged mutation")
    report.update(after=after, recovery_seconds=round(time.monotonic() - started, 3),
                  finished_at=datetime.now(timezone.utc).isoformat())
    save()
    print(output)


if __name__ == "__main__":
    main()
