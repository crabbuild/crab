#!/usr/bin/env python3
"""Qualify offline retention using an aged, disposable RustFS reader fixture."""

import argparse
import hashlib
import json
import re
import subprocess
import uuid
from datetime import datetime, timedelta, timezone
from pathlib import Path

from qualify import command, compose, issue_path, node_url, prove_node, request_json
from qualify_read_replicas import prove_readers, set_reader_target
from render import BUCKET, CONFIG, node_name


def inventory(path: Path) -> dict:
    listed = json.loads(compose(
        path, (), "run", "--rm", "--no-deps", "--entrypoint", "aws", "bucket-init",
        "--endpoint-url", "http://rustfs:9000", "s3api", "list-objects-v2",
        "--bucket", BUCKET, "--prefix", "repositories/cells/v1/",
    ))
    return {item["Key"]: item for item in listed.get("Contents", [])}


def values(port: int) -> dict:
    result = {}
    for index in range(1, 21):
        url = node_url(1, port) + issue_path(index) + "/1"
        issue = request_json("GET", url)
        comments = request_json("GET", url + "/comments?limit=50")
        result[str(index)] = {"title": issue["title"], "body": issue["body"],
                              "comments": sorted(item["body"] for item in comments["items"])}
    return result


def pass_counts(stderr: str) -> dict:
    clean = re.sub(r"\x1b\[[0-9;]*m", "", stderr)
    lines = [line for line in clean.splitlines() if "completed offline Cell retention pass" in line]
    if len(lines) != 1:
        raise RuntimeError("maintenance did not report exactly one completed retention pass")
    counts = dict(re.findall(r"(\w+)=(\d+|true|false)\b", lines[0]))
    return {key: value == "true" if value in ("true", "false") else int(value)
            for key, value in counts.items()}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--runtime-source", required=True)
    parser.add_argument("--resume", action="store_true",
                        help="resume the recorded incomplete one-object sweep")
    parser.add_argument("--node-port-base", type=int, default=18100)
    args = parser.parse_args()
    raw = (args.state / "read-replica-report.json").read_bytes()
    original = json.loads(raw)
    config = json.loads((args.state / "compose.yaml").read_text())
    if config["name"] != original["project"] or original["stages"][-1]["nodes"] != 20:
        raise RuntimeError("requires an existing disposable twenty-node qualification")
    if command("git", "status", "--porcelain"):
        raise RuntimeError("commit the runner before collecting source-bound evidence")
    output = args.state / "reader-retention-report.json"
    if output.exists() and not args.resume:
        raise RuntimeError("retention report exists; preserve it before another run")
    # Other application processes must remain stopped throughout this run.
    # Infrastructure may already be running; never touch another Compose project.
    for index in range(1, 21):
        if compose(args.state / "compose.yaml", ("five", "ten", "twenty"),
                   "ps", "--status", "running", "--quiet", node_name(index)):
            raise RuntimeError("stop this fixture's application nodes before retention qualification")
    image = command("docker", "image", "inspect", "--format", "{{.Id}}", args.image)
    for service in config["services"].values():
        if service.get("image") == original["project"] + ":local":
            service["image"] = image
    maintenance = dict(config["services"]["node-01"])
    maintenance.pop("network_mode")
    maintenance.pop("depends_on")
    maintenance["volumes"] = [volume.replace("node-01-data:", "init-data:")
                              for volume in maintenance["volumes"]]
    config["services"]["maintenance"] = maintenance
    path = args.state / "retention-compose.json"
    path.write_text(json.dumps(config, indent=2) + "\n")
    report = {"runtime_source": command("git", "rev-parse", args.runtime_source),
              "runner_source": command("git", "rev-parse", "HEAD"), "image": image,
              "original_report_sha256": hashlib.sha256(raw).hexdigest(),
              "project": config["name"], "started_at": datetime.now(timezone.utc).isoformat()}
    if args.resume:
        previous = json.loads(output.read_text())
        for key in ("original_report_sha256", "project"):
            if previous[key] != report[key]:
                raise RuntimeError(f"resumed {key} does not match the original receipt")
        if (len(previous.get("passes", [])) != 1 or previous["passes"][0]["counts"]["complete"]
                or previous["passes"][0]["counts"]["deleted_objects"] != 1):
            raise RuntimeError("resume requires exactly one incomplete bounded sweep")
        previous["resumed_by"] = report["runner_source"]
        previous["resumed_at"] = report["started_at"]
        previous["resume_runtime_source"] = report["runtime_source"]
        previous["resume_image"] = image
        report = previous

    def save():
        output.write_text(json.dumps(report, indent=2) + "\n")

    def cli(*arguments):
        return json.loads(compose(path, (), "run", "--rm", "--no-deps", "maintenance",
                                  "--config", CONFIG, "cells", *arguments))

    nodes = [node_name(index) for index in range(1, 4)]
    if not args.resume:
        compose(path, (), "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300",
                "gateway", *nodes)
        report["sessions"] = {node_name(index): prove_node(path, (), index)[0] for index in range(1, 4)}
        set_reader_target(args.node_port_base, 1, 2)
        report["before_values"] = values(args.node_port_base)
        report["serving_readers"] = prove_readers(args.node_port_base, 3, 2)
        report["control_before"] = cli("status", "--owner", "demo", "--name", "work-01")
        report["backup_pin"] = cli("backup", "create", "--pin", uuid.uuid4().hex)
        report["before_inventory"] = inventory(path)
        report["release_before"] = cli("release", "status")
        report["prepared"] = cli("release", "prepare", "--expected-revision",
                                 str(report["release_before"]["revision"]), "--image", "sha256:" + "1" * 64)
        report["passes"] = []
    elif cli("release", "status") != report["passes"][0]["release"]:
        raise RuntimeError("recorded maintenance authority changed before resume")
    prepared = report["prepared"]
    pin = report["backup_pin"]["pin"]

    def check_drained():
        report["drained"] = {}
        for service in nodes:
            container = compose(path, (), "ps", "--all", "--quiet", service)
            state = json.loads(command("docker", "inspect", "--format", "{{json .State}}", container))
            if state["Running"] or state["ExitCode"] != 0 or state["OOMKilled"]:
                raise RuntimeError(f"{service} did not drain successfully before the sweep")
            report["drained"][service] = {"state": state}

    if args.resume:
        check_drained()
    save()
    arguments = ["docker", "compose", "--file", str(path), "run", "--rm", "--no-deps",
                 "maintenance", "--config", CONFIG, "cells", "release", "activate",
                 "--expected-revision", str(prepared["revision"]), "--strategy", "maintenance",
                 "--retention-grace-hours", "1", "--retention-max-deletes"]
    for limit in ((100_000,) if args.resume else (1, 100_000)):
        started = datetime.now(timezone.utc)
        run = subprocess.run([*arguments, str(limit)], text=True, capture_output=True, timeout=600)
        finished = datetime.now(timezone.utc)
        (args.state / f"retention-{limit}.log").write_text(run.stderr + "\n" + run.stdout)
        counts = pass_counts(run.stderr)
        record = {"limit": limit, "started_at": started.isoformat(), "counts": counts,
                  "finished_at": finished.isoformat(),
                  "exit_code": run.returncode, "release": cli("release", "status")}
        report["passes"].append(record)
        save()
        if limit == 1:
            if (run.returncode == 0 or counts["deleted_objects"] != 1 or counts["complete"]
                    or record["release"]["state"] != "maintenance"):
                raise RuntimeError("aged fixture did not exercise an incomplete, fenced retention pass")
            check_drained()
            save()
        elif (run.returncode != 0 or not counts["complete"]
              or record["release"]["state"] != "ready" or counts["retained_pins"] < 1):
            raise RuntimeError("same-revision maintenance retry did not complete with its backup pin")
    # Node inspection uses startup admission and is intentionally unavailable
    # during Maintenance. Check withdrawal after Ready, before any node restarts.
    for service in nodes:
        session = cli("node", "--session", report["sessions"][service], "--json")
        if session["live"]:
            raise RuntimeError("a serving session remained live after retention")
        report["drained"][service]["session"] = session
    after = inventory(path)
    deleted = sorted(report["before_inventory"].keys() - after.keys())
    # Collector time falls inside the command interval. Use its upper bound;
    # objects can legitimately cross the grace cutoff during a long mark scan.
    cutoff = datetime.fromisoformat(report["passes"][-1]["finished_at"]) - timedelta(hours=1)
    if len(deleted) != sum(record["counts"]["deleted_objects"] for record in report["passes"]):
        raise RuntimeError("provider inventory and retention deletion counts differ")
    if any(datetime.fromisoformat(report["before_inventory"][key]["LastModified"]) > cutoff for key in deleted):
        raise RuntimeError("retention deleted an object inside the grace window")
    report["deleted_keys"] = deleted
    report["after_inventory"] = after
    report["verified_pin"] = cli("backup", "verify", "--pin", pin)
    compose(path, (), "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300", *nodes)
    report["after_values"] = values(args.node_port_base)
    if report["before_values"] != report["after_values"]:
        raise RuntimeError("retention changed acknowledged issue data")
    report["recovered_readers"] = prove_readers(args.node_port_base, 3, 2)
    report["control_after"] = cli("status", "--owner", "demo", "--name", "work-01")
    if (report["control_after"]["incarnation"] != report["control_before"]["incarnation"]
            or report["control_after"]["root"]["commit_sequence"]
            < report["control_before"]["root"]["commit_sequence"]):
        raise RuntimeError("retention rewound the acknowledged Cell root")
    body = "Acknowledged after offline reader retention " + uuid.uuid4().hex
    url = node_url(1, args.node_port_base) + issue_path(1) + "/1/comments"
    report["new_comment"] = request_json("POST", url, {"request_id": str(uuid.uuid4()), "body": body})
    if report["new_comment"]["body"] != body or body not in values(args.node_port_base)["1"]["comments"]:
        raise RuntimeError("recovered writer did not preserve its new acknowledgement")
    report["finished_at"] = datetime.now(timezone.utc).isoformat()
    save()
    print(output)


if __name__ == "__main__":
    main()
