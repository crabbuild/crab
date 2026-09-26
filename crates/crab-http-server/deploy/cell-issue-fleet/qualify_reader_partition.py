#!/usr/bin/env python3
"""Partition one reader from RustFS in an existing twenty-node qualification."""

import argparse
import hashlib
import json
import time
import urllib.error
import urllib.request
import uuid
from datetime import datetime, timezone
from pathlib import Path

from qualify import command, compose, issue_path, node_url, prove_node, request_json
from qualify_read_replicas import node_inventory, prove_readers, set_reader_target
from render import CONFIG, node_name


def unavailable(url: str, withdrawn: bool = False) -> dict:
    started = time.monotonic()
    try:
        with urllib.request.urlopen(url + "?read=replica", timeout=10) as response:
            raise RuntimeError(f"isolated reader returned HTTP {response.status}")
    except urllib.error.HTTPError as error:
        raw = error.read()
        # Lease expiry closes the public listener. Only a fresh expired-session
        # proof permits the gateway's empty error in place of the live-node error.
        if error.code == 502 and withdrawn and not raw:
            body = None
        else:
            body = json.loads(raw)
            if (error.code != 503 or not isinstance(body, dict)
                    or body.get("error", {}).get("code") != "replica_unavailable"):
                raise RuntimeError("isolated reader did not fail closed") from error
        return {"status": error.code, "body": body, "seconds": round(time.monotonic() - started, 3)}


def qualify(path: Path, port: int) -> dict:
    profiles = ("five", "ten", "twenty")
    set_reader_target(port, 1, 19)
    sessions = node_inventory(path, profiles, 20)
    args = ("crab-http-server", "--config", CONFIG, "cells", "status",
            "--owner", "demo", "--name", "work-01")
    before = json.loads(compose(path, profiles, "exec", "-T", "node-01", *args))
    owner = node_name(sessions[before["owner"]["session"]][0])
    isolated_index = next(index for index, _ in sessions.values()
                          if index != 1 and node_name(index) != owner)
    isolated = node_name(isolated_index)
    observer_index = next(index for index in range(1, 21)
                          if index != isolated_index and node_name(index) != owner)
    observer = node_name(observer_index)
    config = json.loads(path.read_text())
    proxy = dict(config["services"]["gateway"])
    proxy.pop("healthcheck")
    proxy["depends_on"] = {"fleet-net": {"condition": "service_started"}}
    proxy_file = path.parent / "store-proxy.Caddyfile"
    # Preserve the signed Host and path. Compression is disabled so S3 range
    # reads keep their original byte representation through the HTTP proxy.
    proxy_file.write_text("{\n admin off\n auto_https off\n}\n"
                          "http://127.0.0.1:8190 {\n reverse_proxy http://rustfs:9000 {\n"
                          "  transport http {\n   compression off\n  }\n }\n}\n")
    proxy["volumes"] = [f"{proxy_file}:/etc/caddy/Caddyfile:ro"]
    config["services"]["store-proxy"] = proxy
    config["services"][isolated]["environment"]["AWS_ENDPOINT_URL_S3"] = "http://127.0.0.1:8190"
    partition_path = path.parent / "partition-compose.json"
    partition_path.write_text(json.dumps(config, indent=2) + "\n")
    paused = False
    killed = False
    result = {"isolated_node": isolated, "owner_node": owner, "observer_node": observer}
    try:
        compose(partition_path, profiles, "up", "--detach", "--no-build", "store-proxy")
        compose(partition_path, profiles, "up", "--detach", "--no-build",
                "--wait", "--wait-timeout", "300", isolated)
        isolated_session, _, _ = prove_node(partition_path, profiles, isolated_index)
        status = json.loads(compose(path, profiles, "exec", "-T", observer,
                                   "crab-http-server", "--config", CONFIG,
                                   "cells", "node", "--session", isolated_session, "--json"))
        isolated_id = status["advertisement"]["node"]
        readers = prove_readers(port, 20, 19, ingress=observer_index)
        if isolated_id not in readers["reader_counts"]:
            raise RuntimeError("partition candidate never served a verified replica query")
        before = json.loads(compose(path, profiles, "exec", "-T", observer, *args))
        if node_name(sessions[before["owner"]["session"]][0]) != owner:
            raise RuntimeError("writer changed before partition injection")
        result.update(isolated_session=isolated_session, before=before, warm_readers=readers)
        compose(partition_path, profiles, "pause", "store-proxy")
        paused = True
        isolated_url = node_url(isolated_index, port) + issue_path(1) + "/1"
        result["before_owner_death"] = unavailable(isolated_url)
        with urllib.request.urlopen(node_url(isolated_index, port) + "/livez", timeout=5) as response:
            result["isolated_process_liveness"] = response.status
        compose(path, profiles, "kill", "--signal", "SIGKILL", owner)
        killed = True
        started = time.monotonic()
        observer_url = node_url(observer_index, port) + issue_path(1) + "/1"
        issue = request_json("GET", observer_url)
        after = json.loads(compose(path, profiles, "exec", "-T", observer, *args))
        if (issue["title"] != "Cell issue on node 1"
                or after["owner"]["session"] in (isolated_session, before["owner"]["session"])
                or after["epoch"] <= before["epoch"]
                or after["root"]["commit_sequence"] < before["root"]["commit_sequence"]):
            raise RuntimeError("healthy successor did not recover the authoritative root")
        isolated_status = json.loads(compose(path, profiles, "exec", "-T", observer,
                                            "crab-http-server", "--config", CONFIG,
                                            "cells", "node", "--session", isolated_session, "--json"))
        container = compose(partition_path, profiles, "ps", "--all", "--quiet", isolated)
        state = json.loads(command("docker", "inspect", "--format", "{{json .State}}", container))
        if state["OOMKilled"]:
            raise RuntimeError("partitioned node was killed by memory exhaustion")
        result["isolated_authority_live"] = isolated_status["live"]
        result["isolated_container_state"] = state
        result["after_owner_death"] = unavailable(isolated_url, withdrawn=not isolated_status["live"])
        body = "Acknowledged while a read secondary was partitioned from RustFS"
        comment = request_json("POST", observer_url + "/comments",
                               {"request_id": str(uuid.uuid4()), "body": body})
        if comment.get("body") != body:
            raise RuntimeError("healthy successor did not acknowledge a new write")
        after_write = json.loads(compose(path, profiles, "exec", "-T", observer, *args))
        if (after_write["owner"] != after["owner"] or after_write["epoch"] != after["epoch"]
                or after_write["root"]["commit_sequence"] <= after["root"]["commit_sequence"]):
            raise RuntimeError("healthy successor did not publish the acknowledged write")
        result.update(after=after, after_write=after_write,
                      recovery_and_write_seconds=round(time.monotonic() - started, 3))
    except Exception:
        log = compose(partition_path, profiles, "logs", "--no-color", "--tail", "100", isolated, "store-proxy")
        failure = path.parent / f"partition-failure-{time.time_ns()}.log"
        failure.write_text(log + "\n")
        raise
    finally:
        if paused:
            compose(partition_path, profiles, "unpause", "store-proxy")
        if killed:
            compose(path, profiles, "up", "--detach", "--no-build",
                    "--wait", "--wait-timeout", "300", owner)
        # Restore the original endpoint even when a proof fails. Local volumes
        # remain intact; the old session cannot regain authority after takeover.
        compose(path, profiles, "up", "--detach", "--no-build",
                "--wait", "--wait-timeout", "300", isolated)
        compose(partition_path, profiles, "stop", "store-proxy")
    recovered = request_json("GET", node_url(isolated_index, port) + issue_path(1) + "/1")
    if recovered["title"] != "Cell issue on node 1":
        raise RuntimeError("restored node did not recover ordinary reads")
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", required=True, type=Path)
    parser.add_argument("--node-port-base", type=int, default=18100)
    args = parser.parse_args()
    raw = (args.state / "read-replica-report.json").read_bytes()
    report = json.loads(raw)
    path = args.state / "compose.yaml"
    if report["stages"][-1]["nodes"] != 20 or json.loads(path.read_text())["name"] != report["project"]:
        raise RuntimeError("partition qualification requires the recorded twenty-node project")
    image = command("docker", "image", "inspect", "--format", "{{.Id}}", report["project"] + ":local")
    if image != report["image"] or command("git", "status", "--porcelain"):
        raise RuntimeError("image mismatch or uncommitted runner source")
    receipt = {"runtime_source": report["source_commit"], "image": image,
               "runner_source": command("git", "rev-parse", "HEAD"),
               "original_report_sha256": hashlib.sha256(raw).hexdigest(),
               "started_at": datetime.now(timezone.utc).isoformat()}
    output = args.state / "reader-partition-report.json"
    if output.exists():
        raise RuntimeError("partition receipt already exists; preserve it before another run")
    receipt["partition"] = qualify(path, args.node_port_base)
    receipt["finished_at"] = datetime.now(timezone.utc).isoformat()
    with output.open("x") as stream:
        json.dump(receipt, stream, indent=2)
        stream.write("\n")
    print(output)


if __name__ == "__main__":
    main()
