#!/usr/bin/env python3
"""Lose an acknowledged, unpublished owner while scheduled arrivals continue."""

import argparse
import concurrent.futures
import copy
import json
import queue
import re
import subprocess
import time
import uuid
from collections import Counter, defaultdict
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path

import action_traces
import load
from qualify import BUCKET, CONFIG, ROOT, command, compose, image_provenance, restart_after_fault


def unpublished_acknowledgement(action: dict, control: dict, owner: str, node: dict) -> None:
    """Bind the received HTTP acknowledgement to this owner's unpublished tail."""
    if (action["proof"] != "fleet" or action["owner"] != owner
            or action["cell"] != f"CellId({control['cell']})"
            or action["incarnation"] != f"IncarnationId({control['incarnation']})"
            or action["owner_session"] != f"SessionId({control['owner']['session']})"
            or action["commit_sequence"] <= control["root"]["commit_sequence"]):
        raise RuntimeError("acknowledgement is not an unpublished fleet proof for the selected owner")
    advertisement = node.get("advertisement", {})
    log = advertisement.get("log") or {}
    if (node.get("live") is not True or node.get("session") != control["owner"]["session"]
            or log.get("state") != "open" or log.get("active") is not True
            or type(log.get("epoch")) is not int or log["epoch"] <= 0
            or not log.get("member_nodes") or advertisement.get("node") in log["member_nodes"]):
        raise RuntimeError("acknowledging owner has no active original-follower cohort")


def verify_unique_results(gateway: str, nodes: int, samples: list[dict], run_id: str) -> dict:
    """Check stable request IDs did not create duplicate public application effects."""
    verified = {}
    by_cell = defaultdict(dict)
    for sample in samples:
        expected = by_cell[sample["cell"]]
        if "acknowledged" in sample:
            expected[sample["acknowledged"]["title"]] = sample["acknowledged"]["number"]
    for cell, expected in sorted(by_cell.items()):
        seen = {}
        before = None
        # The list API may return an empty filtered page with a continuation.
        # Follow its strict descending cursor, including beyond the last ack.
        for _ in range(10_000):
            path = load.issue_path(cell) + f"?state=all&limit=50&q=fleet-load-{run_id}-"
            if before is not None:
                path += f"&before={before}"
            response = load.load_request(gateway, nodes, "GET", path)
            if response["outcome"] != "success":
                raise RuntimeError(f"work-{cell:02d} duplicate-result scan failed")
            page = response["body"]
            for item in page["items"]:
                title, number = item["title"], item["number"]
                if (not title.startswith(f"fleet-load-{run_id}-") or title in seen
                        or type(number) is not int or number <= 0
                        or (before is not None and number >= before)):
                    raise RuntimeError(f"work-{cell:02d} has duplicate or invalid load results")
                seen[title] = number
            next_before = page["next"]
            if next_before is None:
                break
            if (type(next_before) is not int or next_before <= 0
                    or (before is not None and next_before >= before)):
                raise RuntimeError("issue-list cursor did not advance")
            before = next_before
        else:
            raise RuntimeError("duplicate-result scan exceeded 10000 pages")
        if any(seen.get(title) != number for title, number in expected.items()):
            raise RuntimeError(f"work-{cell:02d} list lost an acknowledged result")
        verified[cell] = {"unique_results": len(seen), "acknowledged": len(expected)}
    return verified


class TailFault:
    def __init__(self, path: Path, profiles: tuple[str, ...], nodes: int, gateway: str,
                 target: int, owner: str, before: dict, output: Path):
        self.path, self.profiles, self.nodes, self.gateway = path, profiles, nodes, gateway
        self.target, self.owner, self.before, self.output = target, owner, before, output
        self.observer = "node-01" if owner != "node-01" else "node-02"
        self.acknowledgements = queue.Queue(maxsize=1)
        self.receipt = {}
        self.started_at = datetime.now(timezone.utc).isoformat()
        self.policy_installed = False

    def cli(self, *args):
        return compose(self.path, self.profiles, "exec", "-T", self.observer,
                       "crab-http-server", "--config", CONFIG, "cells", *args)

    def status(self):
        return json.loads(self.cli("status", "--owner", "demo", "--name", f"work-{self.target:02d}"))

    def aws(self, operation, *args, missing=None):
        flags = [flag for profile in self.profiles for flag in ("--profile", profile)]
        result = subprocess.run([
            "docker", "compose", "--file", str(self.path), *flags,
            "run", "--rm", "--no-deps", "--entrypoint", "aws", "bucket-init",
            "--endpoint-url", "http://rustfs:9000", "--cli-connect-timeout", "5",
            "--cli-read-timeout", "15", "s3api", operation, "--bucket", BUCKET, *args,
        ], text=True, capture_output=True, timeout=45)
        # The pinned AWS CLI exposes service error codes in its legacy error
        # envelope. A transport failure must never prove an intentional deny.
        if result.returncode:
            if missing and f"({missing})" in result.stderr:
                return None
            raise subprocess.CalledProcessError(result.returncode, result.args, result.stdout, result.stderr)
        return result.stdout

    def install_policy(self):
        if self.aws("get-bucket-policy", missing="NoSuchBucketPolicy") is not None:
            raise RuntimeError("fault requires an isolated fixture bucket without an existing policy")
        cell, incarnation = self.before["cell"], self.before["incarnation"]
        if not re.fullmatch(r"[0-9a-f]{64}", cell) or not re.fullmatch(r"[0-9a-f]{32}", incarnation):
            raise RuntimeError("fault target has an invalid Cell identity")
        resource = f"repositories/cells/v1/apps/*/cells/{cell}/inc/{incarnation}/objects/*"
        policy = {"Version": "2012-10-17", "Statement": [{
            "Effect": "Deny", "Principal": "*", "Action": "s3:PutObject",
            "Resource": f"arn:aws:s3:::{BUCKET}/{resource}",
        }]}
        # Set before dispatch: an uncertain policy PUT still requires cleanup.
        self.policy_installed = True
        self.aws("put-bucket-policy", "--policy", json.dumps(policy))
        probe = resource.replace("*", "0" * 32, 1).removesuffix("*") + "fault-policy-probe"
        if self.aws("put-object", "--key", probe, "--body", "/etc/hosts", missing="AccessDenied") is not None:
            self.aws("delete-object", "--key", probe)
            raise RuntimeError("RustFS did not reject the selected Cell's immutable writes")
        self.receipt["immutable_object_put_rejected"] = True

    def clear_policy(self):
        if self.policy_installed:
            self.aws("delete-bucket-policy")
            self.policy_installed = False

    @contextmanager
    def publication_denied(self):
        failed = False
        try:
            self.install_policy()
            yield
        except BaseException:
            failed = True
            raise
        finally:
            try:
                self.clear_policy()
            except Exception as error:
                self.receipt["policy_cleanup"] = {"passed": False, "error": f"{type(error).__name__}: {error}"}
                if not failed:
                    raise
            else:
                self.receipt["policy_cleanup"] = {"passed": True}

    def on_acknowledged(self, sample):
        if sample["cell"] == self.target:
            try:
                # The caller appends its immediate read next. The fault owns a
                # snapshot of the completed write, never a concurrently edited pair.
                self.acknowledgements.put_nowait(copy.deepcopy(sample))
            except queue.Full:
                pass

    def trace_events(self, stage):
        destination = self.output / stage
        destination.mkdir()
        events = []
        for index in range(1, self.nodes + 1):
            node = load.node_name(index)
            text = compose(self.path, self.profiles, "logs", "--no-color", "--no-log-prefix",
                           "--since", self.started_at, node)
            (destination / f"{node}.log").write_text(text)
            events.extend(action_traces.parse_log(text, node))
        return events

    def run(self):
        try:
            sample = self.acknowledgements.get(timeout=45)
        except queue.Empty as error:
            raise RuntimeError("no target acknowledgement arrived while immutable writes were denied") from error
        action = action_traces.join([sample], self.trace_events("before-kill"))[0]
        control = self.status()
        if (control.get("state") != "serving" or control["owner"] != self.before["owner"]
                or control["epoch"] != self.before["epoch"]):
            raise RuntimeError("target owner changed before the fault")
        node = json.loads(self.cli("node", "--session", control["owner"]["session"], "--json"))
        unpublished_acknowledgement(action, control, self.owner, node)
        metrics = compose(self.path, self.profiles, "exec", "-T", self.owner,
                          "crab-http-server", "--config", CONFIG, "cells", "metrics")
        uncovered = next((int(line.split()[1]) for line in metrics.splitlines()
                          if line.startswith("crab_cell_node_log_uncovered_bytes ")), 0)
        if uncovered <= 0:
            raise RuntimeError("the acknowledging owner has no retained unpublished tail")
        project = json.loads(self.path.read_text())["name"]
        identities = {}
        for index in range(1, self.nodes + 1):
            name = load.node_name(index)
            identity = compose(self.path, self.profiles, "exec", "-T", name,
                               "cat", "/var/lib/crab/cells/node-id").strip()
            identities[identity] = name
        members = node["advertisement"]["log"]["member_nodes"]
        if any(member not in identities or identities[member] == self.owner for member in members):
            raise RuntimeError("an original follower is unavailable before the fault")
        container = command("docker", "ps", "--quiet", "--filter", f"label=com.docker.compose.project={project}",
                            "--filter", f"label=com.docker.compose.service={self.owner}")
        if not re.fullmatch(r"[0-9a-f]{12,64}", container):
            raise RuntimeError("fault owner is not exactly one running container")
        inspected = json.loads(command("docker", "inspect", container))[0]
        mounts = [mount for mount in inspected["Mounts"] if mount["Destination"] == "/var/lib/crab/cells"]
        if len(mounts) != 1 or mounts[0]["Type"] != "volume":
            raise RuntimeError("owner loss requires the fixture's disposable named Cell volume")
        volume = mounts[0]["Name"]
        labels = json.loads(command("docker", "volume", "inspect", volume))[0].get("Labels") or {}
        if labels.get("com.docker.compose.project") != project:
            raise RuntimeError("owner volume is not owned by the selected fixture")
        # A fully covered cohort can rotate during evidence collection. Bind
        # the last observation to the acknowledged log before losing its owner.
        last_control = self.status()
        last_node = json.loads(self.cli("node", "--session", control["owner"]["session"], "--json"))
        unpublished_acknowledgement(action, last_control, self.owner, last_node)
        last_log = last_node["advertisement"]["log"]
        if (last_control["owner"] != control["owner"] or last_control["epoch"] != control["epoch"]
                or last_control.get("state") != "serving"
                or last_log["epoch"] != node["advertisement"]["log"]["epoch"]
                or set(last_log["member_nodes"]) != set(members)):
            raise RuntimeError("acknowledging owner or follower cohort changed before kill")
        self.receipt.update(acknowledgement=sample, action=action, control_before=control,
                            node_before=node, uncovered_bytes=uncovered,
                            control_pre_kill=last_control, node_pre_kill=last_node,
                            followers={member: identities[member] for member in members})
        command("docker", "kill", "--signal", "KILL", container)
        self.receipt["killed_ns"] = time.monotonic_ns()
        # Capture the final owner trace before removing both the process and its
        # volume. Recovery cannot accidentally consult the failed writer's disk.
        with (self.output / "failed-owner.log").open("x") as log:
            subprocess.run(["docker", "logs", container], stdout=log, stderr=subprocess.STDOUT, check=True)
        command("docker", "rm", container)
        command("docker", "volume", "rm", volume)
        self.receipt["owner_disk_removed"] = True
        self.clear_policy()
        deadline = time.monotonic() + 120
        issue = sample["acknowledged"]
        while time.monotonic() < deadline:
            try:
                observed = load.request(self.gateway, self.nodes, "GET",
                                        load.issue_path(self.target) + f"/{issue['number']}")
                current = self.status()
            except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError):
                time.sleep(0.5)
                continue
            successor = (current.get("owner") or {}).get("session")
            if not successor or successor == control["owner"]["session"] or current.get("state") != "serving":
                time.sleep(0.5)
                continue
            if observed["body"].get("number") != issue["number"] or observed["body"].get("title") != issue["title"]:
                raise RuntimeError("takeover lost the acknowledged unpublished result")
            if (current["epoch"] <= control["epoch"]
                    or current["root"]["commit_sequence"] < action["commit_sequence"]):
                raise RuntimeError("takeover did not cover the acknowledged commit position")
            self.receipt.update(recovered_ns=time.monotonic_ns(), control_after=current)
            return
        raise RuntimeError("unpublished-tail recovery did not complete within 120 seconds")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--nodes", type=int, choices=(3, 5, 10, 20), required=True)
    parser.add_argument("--gateway-port", type=int, default=18080)
    parser.add_argument("--cells", type=int, default=20)
    parser.add_argument("--rate", type=float, default=5)
    parser.add_argument("--duration", type=float, default=180)
    parser.add_argument("--max-in-flight", type=int, default=64)
    parser.add_argument("--hot-share", type=float, default=0)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    workload = load.Workload(args.cells, args.rate, args.duration, args.max_in_flight, args.hot_share)
    if workload.cells < 2 or workload.duration < 60:
        parser.error("fault qualification requires at least two Cells and 60 seconds of arrivals")
    output = args.output.expanduser().resolve()
    output.mkdir()
    path = args.state.expanduser().resolve() / "compose.yaml"
    deployment = json.loads(path.read_text())
    project = deployment["name"]
    expected = {load.node_name(index) for index in range(1, args.nodes + 1)}
    if not re.fullmatch(r"crab-cell-issue-[a-z0-9-]+", project) or load.running_nodes(project) != expected:
        raise RuntimeError("fault requires exactly the selected isolated fleet nodes")
    profiles = load.profiles_for(args.nodes)
    gateway = f"http://127.0.0.1:{args.gateway_port}"
    server = image_provenance(deployment["services"]["node-01"]["image"])
    if any(deployment["services"][name]["image"] != server["image"] for name in expected):
        raise RuntimeError("all node services must pin the same server image ID; run qualify.py first")
    owners, controls = load.owner_map(path, profiles, args.nodes, args.cells)
    target = 1
    unaffected = {cell for cell, owner in owners.items() if owner != owners[target]}
    if not unaffected:
        raise RuntimeError("fault requires Cells on another owner to measure unaffected service")
    if not load.drain_publication(path, profiles, args.nodes)["drained"]:
        raise RuntimeError("baseline publication did not drain before the fault workload")
    fault = TailFault(path, profiles, args.nodes, gateway, target, owners[target], controls[target], output)
    run_id = uuid.uuid4().hex[:12]
    report = {"schema": 1, "server": server, "project": project,
              "source": command("git", "-C", str(ROOT), "rev-parse", "HEAD"),
              "source_dirty": bool(command("git", "-C", str(ROOT), "status", "--porcelain")),
              "workload": vars(workload), "run_id": run_id,
              "owners_before": owners, "fault": fault.receipt, "passed": False}
    restart = lambda: compose(path, profiles, "up", "--detach", "--no-build", "--wait", "--wait-timeout", "300", owners[target])
    try:
        with restart_after_fault(fault.receipt, restart):
            with fault.publication_denied():
                with (output / "samples.jsonl").open("x") as raw, concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
                    failure = executor.submit(fault.run)
                    summary, samples = load.scheduled_load(gateway, args.nodes, workload, run_id, raw, fault.on_acknowledged)
                    report["load"] = summary
                    failure.result()
            report["acknowledgements"] = load.verify_acknowledged(gateway, args.nodes, samples)
            report["unique_results"] = verify_unique_results(gateway, args.nodes, samples, run_id)
            killed, recovered = fault.receipt["killed_ns"], fault.receipt["recovered_ns"]
            during = [sample for sample in samples if killed <= sample.get("started_ns", 0) <= recovered]
            healthy = [sample for sample in during if sample["cell"] in unaffected]
            after = [sample for sample in samples if sample.get("started_ns", 0) > recovered]
            report["during_recovery"] = {
                "pairs": len(during), "initially_other_owner_outcomes": dict(Counter(sample["outcome"] for sample in healthy)),
                "initially_other_owner_pair_latency": load.percentiles([sample["scheduled_latency_ms"] for sample in healthy
                                                                       if sample["outcome"] == "success"]),
                "after_recovery_pairs": len(after),
            }
            if (summary["stopped_on_invariant"] or not healthy or not after
                    or any(sample["outcome"] != "success" for sample in healthy)):
                raise RuntimeError("arrivals did not span recovery with unaffected-Cell progress")
        report["publication_drain"] = load.drain_publication(path, profiles, args.nodes)
        if not report["publication_drain"]["drained"]:
            raise RuntimeError("publication backlog did not drain after recovery")
        report["passed"] = True
    except BaseException as error:
        report["error"] = f"{type(error).__name__}: {error}"
        report["passed"] = False
        raise
    finally:
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(output / "report.json")


if __name__ == "__main__":
    main()
