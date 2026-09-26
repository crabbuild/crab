#!/usr/bin/env python3
"""Measure scheduled Cell actions through every load-balanced entry node."""

import argparse
import concurrent.futures
import json
import math
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
import uuid
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

sys.dont_write_bytecode = True

from qualify import BUCKET, CONFIG, MEMORY_LIMIT, ROOT, command, compose, image_provenance, issue_path, node_name, prove_node
import action_traces


class RequestFailure(RuntimeError):
    def __init__(self, status: int, detail: str, http_request_id: str | None):
        super().__init__(detail)
        self.status = status
        self.http_request_id = http_request_id


@dataclass(frozen=True)
class Workload:
    cells: int
    rate: float
    duration: float
    max_in_flight: int
    hot_share: float

    def __post_init__(self):
        if not 1 <= self.cells <= 1000 or not 1 <= self.max_in_flight <= 256:
            raise ValueError("cells must be 1..1000 and max-in-flight must be 1..256")
        if not all(math.isfinite(x) for x in (self.rate, self.duration, self.hot_share)):
            raise ValueError("workload numbers must be finite")
        if self.rate <= 0 or self.duration <= 0 or not 0 < self.rate * self.duration <= 100_000:
            raise ValueError("rate and duration must be positive and schedule 1..100000 pairs")
        if not 0 <= self.hot_share <= 1:
            raise ValueError("hot-share must be 0..1")

    def cell(self, index: int) -> int:
        if self.cells == 1 or self.hot_share == 0:
            return index % self.cells + 1
        hot_before = math.floor(index * self.hot_share)
        hot_after = math.floor((index + 1) * self.hot_share)
        if hot_after > hot_before:
            return 1
        return 2 + (index - hot_before) % (self.cells - 1)


def profiles_for(nodes: int) -> tuple[str, ...]:
    return {3: (), 5: ("five",), 10: ("five", "ten"), 20: ("five", "ten", "twenty")}[nodes]


def running_nodes(project: str) -> set[str]:
    services = command(
        "docker", "ps", "--filter", f"label=com.docker.compose.project={project}",
        "--format", '{{.Label "com.docker.compose.service"}}',
    ).splitlines()
    return {service for service in services if re.fullmatch(r"node-\d{2}", service)}


def status(path: Path, profiles: tuple[str, ...], cell: int) -> dict:
    result = compose(
        path, profiles, "exec", "-T", "node-01", "crab-http-server", "--config",
        CONFIG, "cells", "status", "--owner", "demo", "--name", f"work-{cell:02d}",
    )
    value = json.loads(result)
    if value.get("state") != "serving" or not value.get("root"):
        raise RuntimeError(f"work-{cell:02d} has no serving owner or published root")
    return value


def server_request_id(headers, required: bool) -> str | None:
    value = headers.get("x-request-id")
    if value is None and not required:
        return None
    try:
        return str(uuid.UUID(value))
    except (ValueError, TypeError, AttributeError) as error:
        raise RuntimeError("HTTP response has no valid server request ID") from error


def request(gateway: str, nodes: int, method: str, path: str, payload: dict | None = None) -> dict:
    body = json.dumps(payload).encode() if payload is not None else None
    headers = {"content-type": "application/json"} if body is not None else {}
    started = time.monotonic()
    try:
        with urllib.request.urlopen(
            urllib.request.Request(gateway + path, data=body, method=method, headers=headers),
            timeout=30,
        ) as response:
            expected = 201 if method == "POST" else 200
            if response.status != expected:
                raise RuntimeError(f"{method} {path} returned {response.status}, expected {expected}")
            http_request_id = server_request_id(response.headers, required=True)
            upstream = response.headers.get("X-Crab-Fleet-Entry", "")
            match = re.fullmatch(r"127\.0\.0\.1:(\d+)", upstream)
            entry = int(match.group(1)) - 8200 if match else 0
            if not 1 <= entry <= nodes:
                raise RuntimeError(f"{method} {path} used an unexpected entry node: {upstream!r}")
            result = json.load(response)
            if not isinstance(result, dict):
                raise RuntimeError(f"{method} {path} returned a non-object JSON body")
    except urllib.error.HTTPError as error:
        with error:
            http_request_id = server_request_id(error.headers, required=False)
            detail = error.read(512).decode(errors="replace")
        raise RequestFailure(error.code, f"{method} {path} returned {error.code}: {detail}", http_request_id) from error
    return {
        "entry": node_name(entry),
        "status": expected,
        "http_request_id": http_request_id,
        "latency_ms": (time.monotonic() - started) * 1_000,
        "body": result,
    }


def load_request(gateway: str, nodes: int, method: str, path: str, payload: dict | None = None) -> dict:
    started = time.monotonic()
    failures = []
    attempts = []
    for attempt in range(6):
        attempt_started = time.monotonic()
        http_request_id = None
        status = None
        try:
            sample = request(gateway, nodes, method, path, payload)
            attempts.append({key: sample[key] for key in ("status", "http_request_id", "latency_ms")})
            sample["latency_ms"] = (time.monotonic() - started) * 1_000
            sample["retries"] = len(failures)
            sample["retry_reasons"] = failures
            sample["attempts"] = attempts
            sample["outcome"] = "success"
            return sample
        except RequestFailure as error:
            http_request_id = error.http_request_id
            status = error.status
            failures.append(error.status)
            detail = str(error)
            retryable = error.status in (429, 502, 503, 504)
        except OSError as error:
            failures.append("transport")
            detail = str(error)
            retryable = True
        except (RuntimeError, ValueError) as error:
            failures.append("contract")
            detail = str(error)
            retryable = False
        attempts.append({
            "status": status, "http_request_id": http_request_id,
            "latency_ms": (time.monotonic() - attempt_started) * 1_000,
        })
        if not retryable or attempt == 5:
            return {
                "outcome": "contract_error" if failures[-1] == "contract" else "failed",
                "latency_ms": (time.monotonic() - started) * 1_000,
                "retries": attempt,
                "retry_reasons": failures,
                "attempts": attempts,
                "error": detail,
            }
        time.sleep(0.1 * 2 ** attempt)
    raise RuntimeError("load request retry loop did not terminate")


def percentiles(samples: list[float]) -> dict:
    if not samples:
        return {"count": 0}
    ordered = sorted(samples)
    result = {
        f"p{percentile}_ms": round(ordered[math.ceil(len(ordered) * percentile / 100) - 1], 3)
        for percentile in (50, 95, 99)
    }
    result["max_ms"] = round(ordered[-1], 3)
    result["count"] = len(samples)
    return result


def verify_acknowledged(gateway: str, nodes: int, samples: list[dict]) -> dict:
    acknowledged = [sample for sample in samples if "acknowledged" in sample]
    started = time.monotonic()

    def verify(sample):
        issue = sample["acknowledged"]
        observed = load_request(gateway, nodes, "GET", issue_path(sample["cell"]) + f"/{issue['number']}")
        body = observed.get("body", {})
        if (observed["outcome"] != "success" or body.get("number") != issue["number"]
                or body.get("title") != issue["title"]):
            raise RuntimeError(
                f"work-{sample['cell']:02d} lost or changed acknowledgement {sample['request_id']} "
                f"(issue {issue['number']}): {observed.get('error', 'result mismatch')}"
            )

    # Verify outside the timed arrival phase, with bounded work even at the
    # 100,000-arrival ceiling. Keep earlier results, not just each Cell's latest.
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
        for start in range(0, len(acknowledged), 8):
            for _ in executor.map(verify, acknowledged[start:start + 8]):
                pass
    return {
        "verified": len(acknowledged),
        "by_cell": dict(Counter(sample["cell"] for sample in acknowledged)),
        "elapsed_seconds": round(time.monotonic() - started, 3),
    }


def cover_routes(gateway: str, nodes: int, cells: int) -> tuple[dict, list[dict]]:
    coverage = {}
    samples = []
    expected = {node_name(index) for index in range(1, nodes + 1)}
    for cell in range(1, cells + 1):
        seen = set()
        for _ in range(nodes * 4):
            sample = request(gateway, nodes, "GET", issue_path(cell) + "/1")
            if sample["body"].get("number") != 1:
                raise RuntimeError(f"work-{cell:02d} did not return its original issue")
            seen.add(sample["entry"])
            samples.append({"cell": cell, "entry": sample["entry"], "latency_ms": sample["latency_ms"]})
            if seen == expected:
                break
        if seen != expected:
            raise RuntimeError(f"work-{cell:02d} missed entry nodes: {sorted(expected - seen)}")
        coverage[f"work-{cell:02d}"] = sorted(seen)
    return coverage, samples


def load_pair(gateway: str, nodes: int, cell: int, arrival: int, run_id: str, scheduled: float) -> dict:
    title = f"fleet-load-{run_id}-{cell:02d}-{arrival:06d}"
    request_id = str(uuid.uuid5(uuid.NAMESPACE_URL, title))
    result = {
        "cell": cell, "arrival": arrival, "request_id": request_id,
        "dispatch_delay_ms": (time.monotonic() - scheduled) * 1_000,
        "operations": [],
    }
    created = load_request(gateway, nodes, "POST", issue_path(cell), {
        "request_id": request_id, "title": title, "body": "Load-balanced durable Cell issue",
    })
    body = created.pop("body", {})
    result["operations"].append({"operation": "write", **created})
    result["outcome"] = created["outcome"]
    if created["outcome"] == "success":
        number = body.get("number")
        if type(number) is not int or number < 1 or body.get("title") != title:
            result.update(outcome="contract_error", error="unexpected issue creation result")
        else:
            result["acknowledged"] = {"number": number, "title": title}
            observed = load_request(gateway, nodes, "GET", issue_path(cell) + f"/{number}")
            body = observed.pop("body", {})
            result["operations"].append({"operation": "read", **observed})
            result["outcome"] = observed["outcome"]
            if observed["outcome"] == "success" and body.get("title") != title:
                result.update(outcome="contract_error", error="acknowledged issue readback mismatch")
            elif observed["outcome"] == "failed" and observed["retry_reasons"][-1] == 404:
                result.update(outcome="contract_error", error="acknowledged issue disappeared")
    result["scheduled_latency_ms"] = (time.monotonic() - scheduled) * 1_000
    return result


def scheduled_load(gateway: str, nodes: int, workload: Workload, run_id: str, raw) -> tuple[dict, list[dict]]:
    count = math.ceil(workload.rate * workload.duration)
    samples = []
    started = time.monotonic()
    stopped = False
    peak = 0

    def record(sample):
        nonlocal stopped
        raw.write(json.dumps(sample) + "\n")
        raw.flush()
        samples.append(sample)
        stopped |= sample["outcome"] == "contract_error"

    with concurrent.futures.ThreadPoolExecutor(max_workers=workload.max_in_flight) as executor:
        pending = set()
        for arrival in range(count):
            scheduled = started + arrival / workload.rate
            time.sleep(max(0, scheduled - time.monotonic()))
            completed = {future for future in pending if future.done()}
            pending -= completed
            for future in completed:
                record(future.result())
            if stopped:
                break
            cell = workload.cell(arrival)
            late = time.monotonic() - scheduled
            # Never accumulate an executor queue or catch up by issuing a burst.
            # Missed arrivals remain explicit, so overload cannot lower offered load.
            outcome = "scheduler_late" if late >= 1 / workload.rate else "client_capacity"
            if late >= 1 / workload.rate or len(pending) == workload.max_in_flight:
                record({"arrival": arrival, "cell": cell, "outcome": outcome,
                        "dispatch_delay_ms": late * 1_000, "operations": []})
                continue
            pending.add(executor.submit(load_pair, gateway, nodes, cell, arrival, run_id, scheduled))
            peak = max(peak, len(pending))
        for future in concurrent.futures.as_completed(pending):
            record(future.result())
    if not stopped:
        time.sleep(max(0, started + workload.duration - time.monotonic()))
    elapsed = time.monotonic() - started
    return {
        "planned_pairs": count,
        "offered_pairs": len(samples),
        "admitted_pairs": sum(sample["outcome"] not in ("client_capacity", "scheduler_late") for sample in samples),
        "outcomes": dict(Counter(sample["outcome"] for sample in samples)),
        "peak_in_flight": peak,
        "arrival_seconds": workload.duration,
        "elapsed_seconds": elapsed,
        "drain_seconds": max(0, elapsed - workload.duration),
        "stopped_on_invariant": stopped,
    }, samples


def owner_map(path: Path, profiles: tuple[str, ...], nodes: int, cells: int) -> tuple[dict, dict]:
    sessions = {}
    for index in range(1, nodes + 1):
        session, _, _ = prove_node(path, profiles, index)
        sessions[session] = node_name(index)
    statuses = {cell: status(path, profiles, cell) for cell in range(1, cells + 1)}
    owners = {}
    for cell, value in statuses.items():
        session = value.get("owner", {}).get("session")
        if session not in sessions:
            raise RuntimeError(f"work-{cell:02d} has no live Compose owner")
        owners[cell] = sessions[session]
    return owners, statuses


def verify_roots(path: Path, profiles: tuple[str, ...], before: dict, cells) -> dict:
    after = {}
    for cell in cells:
        baseline = before[cell]["root"]["commit_sequence"]
        deadline = time.monotonic() + 60
        while True:
            observed = status(path, profiles, cell)
            if observed["root"]["commit_sequence"] > baseline:
                after[cell] = observed
                break
            if time.monotonic() >= deadline:
                raise RuntimeError(f"work-{cell:02d} did not publish a newer RustFS root")
            time.sleep(0.5)
    return after


def drain_publication(path: Path, profiles: tuple[str, ...], nodes: int) -> dict:
    started = time.monotonic()
    samples = []
    while True:
        uncovered = {}
        for index in range(1, nodes + 1):
            node = node_name(index)
            metrics = compose(path, profiles, "exec", "-T", node, "crab-http-server", "--config", CONFIG, "cells", "metrics")
            value = next((line.split()[-1] for line in metrics.splitlines()
                          if line.startswith("crab_cell_node_log_uncovered_bytes ")), None)
            if value is None:
                raise RuntimeError(f"{node} did not expose publication backlog")
            count = float(value)
            if not math.isfinite(count) or count < 0 or not count.is_integer():
                raise RuntimeError(f"{node} exposed invalid publication backlog: {value}")
            uncovered[node] = int(count)
        elapsed = time.monotonic() - started
        samples.append({"elapsed_seconds": elapsed, "uncovered_bytes": uncovered})
        if all(value == 0 for value in uncovered.values()):
            return {"drained": True, "elapsed_seconds": elapsed, "samples": samples}
        if elapsed >= 120:
            return {"drained": False, "elapsed_seconds": elapsed, "samples": samples}
        time.sleep(1)


def observe_nodes(path: Path, profiles: tuple[str, ...], nodes: int, stop, output) -> dict:
    names = [node_name(index) for index in range(1, nodes + 1)]
    flags = [flag for profile in profiles for flag in ("--profile", profile)]
    prefix = ["docker", "compose", "--file", str(path), *flags]
    count = 0
    errors = []

    def read(*args):
        return subprocess.run(args, check=True, capture_output=True, text=True, timeout=15).stdout

    def metrics(node):
        return read(*prefix, "exec", "-T", node, "crab-http-server", "--config", CONFIG, "cells", "metrics")

    with concurrent.futures.ThreadPoolExecutor(max_workers=min(4, nodes)) as readers:
        while True:
            sample = {"started_at": datetime.now(timezone.utc).isoformat()}
            started = time.monotonic()
            try:
                containers = read(*prefix, "ps", "--quiet", *names).split()
                if len(containers) != nodes:
                    raise RuntimeError("resource observation lost an expected node")
                sample["containers"] = [json.loads(line) for line in read(
                    "docker", "stats", "--no-stream", "--format", "{{json .}}", *containers,
                ).splitlines()]
                sample["metrics"] = dict(zip(names, readers.map(metrics, names)))
            except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
                sample["error"] = str(error)
                errors.append(str(error))
            sample["elapsed_seconds"] = time.monotonic() - started
            output.write(json.dumps(sample) + "\n")
            output.flush()
            count += 1
            if stop.wait(5):
                return {"samples": count, "errors": errors}


def recover_owner(
    path: Path, profiles: tuple[str, ...], gateway: str, nodes: int,
    owner: str, before: dict, latest: dict, target: int, samples: list[dict],
) -> dict:
    observer = "node-01" if owner != "node-01" else "node-02"
    for _ in range(60):
        metrics = compose(path, profiles, "exec", "-T", owner, "crab-http-server", "--config", CONFIG, "cells", "metrics")
        uncovered = next(
            (line.split()[-1] for line in metrics.splitlines()
             if line.startswith("crab_cell_node_log_uncovered_bytes ")),
            None,
        )
        if uncovered == "0":
            break
        time.sleep(1)
    else:
        raise RuntimeError("owner did not publish its retained bytes to RustFS")
    live = status(path, profiles, target)
    if live["owner"]["session"] != before["owner"]["session"]:
        raise RuntimeError("owner moved before the owner-loss fault")
    before = live
    started = time.monotonic()
    try:
        compose(path, profiles, "kill", "--signal", "SIGKILL", owner)
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            try:
                observed = request(gateway, nodes, "GET", issue_path(target) + f"/{latest['number']}")
                current = json.loads(compose(
                    path, profiles, "exec", "-T", observer, "crab-http-server", "--config",
                    CONFIG, "cells", "status", "--owner", "demo", "--name", f"work-{target:02d}",
                ))
            except (OSError, RuntimeError, subprocess.CalledProcessError, ValueError):
                time.sleep(1)
                continue
            old_session = before["owner"]["session"]
            new_session = (current.get("owner") or {}).get("session")
            root = current.get("root") or {}
            if new_session == old_session or not new_session:
                time.sleep(1)
                continue
            if observed["body"].get("title") != latest["title"]:
                raise RuntimeError("recovered owner lost an acknowledged issue")
            if (root.get("commit_sequence", -1) < before["root"]["commit_sequence"]
                    or root.get("txid", -1) < before["root"]["txid"]):
                raise RuntimeError("recovered owner regressed the published RustFS root")
            return {
                "lost_node": owner,
                "new_session": new_session,
                "recovery_seconds": round(time.monotonic() - started, 3),
                "same_root": root == before["root"],
                "entry_node": observed["entry"],
                "acknowledgements": verify_acknowledged(gateway, nodes, samples),
            }
        raise RuntimeError("load-balanced owner recovery did not complete in 120 seconds")
    finally:
        compose(path, profiles, "up", "--detach", "--no-build", "--wait", owner)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--nodes", type=int, choices=(3, 5, 10, 20), required=True)
    parser.add_argument("--gateway-port", type=int, default=18080)
    parser.add_argument("--cells", type=int, default=20)
    parser.add_argument("--rate", type=float, default=5, help="scheduled create/read pairs per second")
    parser.add_argument("--duration", type=float, default=60)
    parser.add_argument("--max-in-flight", type=int, default=64)
    parser.add_argument("--hot-share", type=float, default=0, help="fraction directed to Cell 1; zero is uniform")
    parser.add_argument("--output", type=Path, help="report path instead of a generated name")
    args = parser.parse_args()
    try:
        workload = Workload(args.cells, args.rate, args.duration, args.max_in_flight, args.hot_share)
    except ValueError as error:
        parser.error(str(error))
    path = args.state.expanduser().resolve() / "compose.yaml"
    deployment = json.loads(path.read_text())
    project = deployment["name"]
    run_id = uuid.uuid4().hex[:12]
    output = args.output.expanduser().resolve() if args.output else path.parent / f"load-{args.nodes}-{run_id}.json"
    raw_path = output.with_suffix(".samples.jsonl")
    nodes_path = output.with_suffix(".nodes.jsonl")
    traces_path = output.with_suffix(".traces")
    if any(candidate.exists() for candidate in (output, raw_path, nodes_path, traces_path)):
        raise RuntimeError(f"load report or samples already exist: {output}")
    expected_nodes = {node_name(index) for index in range(1, args.nodes + 1)}
    active_nodes = running_nodes(project)
    if active_nodes != expected_nodes:
        raise RuntimeError(f"expected exactly {args.nodes} running Cell nodes, found {sorted(active_nodes)}")
    profiles = profiles_for(args.nodes)
    server = image_provenance(deployment["services"]["node-01"]["image"])
    if any(deployment["services"][name]["image"] != server["image"] for name in expected_nodes):
        raise RuntimeError("all node services must pin the same server image ID; run qualify.py first")
    gateway = f"http://127.0.0.1:{args.gateway_port}"
    _, before = owner_map(path, profiles, args.nodes, args.cells)
    coverage, coverage_samples = cover_routes(gateway, args.nodes, args.cells)
    report = {
        "schema": 4,
        "source": command("git", "-C", str(ROOT), "rev-parse", "HEAD"),
        "source_role": "load_generator",
        "source_dirty": bool(command("git", "-C", str(ROOT), "status", "--porcelain")),
        "project": project,
        "server": server,
        "rustfs": {
            "image": deployment["services"]["rustfs"]["image"],
            "image_id": command("docker", "image", "inspect", "--format", "{{.Id}}", deployment["services"]["rustfs"]["image"]),
            "bucket": BUCKET,
            "endpoint": "http://rustfs:9000",
        },
        "compose_profiles": list(profiles),
        "node_cpu_limit": deployment["services"]["node-01"]["cpus"],
        "node_memory_limit_bytes": MEMORY_LIMIT,
        "trace_filters": {name: deployment["services"][name]["environment"].get("RUST_LOG") for name in sorted(expected_nodes)},
        "started_at": datetime.now(timezone.utc).isoformat(),
        "nodes": args.nodes,
        "workload": vars(workload),
        "raw_samples": raw_path.name,
        "node_samples": nodes_path.name,
        "coverage_requests": len(coverage_samples),
        "node_cell_coverage": coverage,
        "coverage_read_latency": percentiles([sample["latency_ms"] for sample in coverage_samples]),
        "passed": False,
    }
    try:
        with raw_path.open("x") as raw, nodes_path.open("x") as node_samples:
            stop = threading.Event()
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as observer:
                observations = observer.submit(observe_nodes, path, profiles, args.nodes, stop, node_samples)
                try:
                    summary, samples = scheduled_load(gateway, args.nodes, workload, run_id, raw)
                finally:
                    stop.set()
                    report["node_observations"] = observations.result()
        operations = [{"cell": sample["cell"], **operation}
                      for sample in samples for operation in sample["operations"]]
        successes = [operation for operation in operations if operation["outcome"] == "success"]
        entries = Counter(operation["entry"] for operation in successes)
        acknowledged = {}
        for sample in samples:
            if "acknowledged" in sample:
                cell = sample["cell"]
                issue = sample["acknowledged"]
                if cell not in acknowledged or issue["number"] > acknowledged[cell]["number"]:
                    acknowledged[cell] = issue
        expected = len(successes) / args.nodes
        balanced = (set(entries) == expected_nodes and min(entries.values()) >= expected * 0.7
                    and max(entries.values()) <= expected * 1.3)
        report.update(
            load=summary,
            logical_requests=len(operations),
            successful_requests=len(successes),
            successful_requests_per_second=len(successes) / summary["elapsed_seconds"],
            entry_requests=dict(sorted(entries.items())),
            ingress_balanced=balanced,
            cell_offered_pairs=dict(Counter(sample["cell"] for sample in samples)),
            total_retries=sum(operation["retries"] for operation in operations),
            attempt_failures=dict(Counter(str(reason) for operation in operations for reason in operation["retry_reasons"])),
            latency={operation: percentiles([sample["latency_ms"] for sample in successes if sample["operation"] == operation])
                     for operation in ("write", "read")},
            scheduled_pair_latency=percentiles([sample["scheduled_latency_ms"] for sample in samples if "scheduled_latency_ms" in sample]),
            dispatch_delay=percentiles([sample["dispatch_delay_ms"] for sample in samples]),
        )
        if summary["stopped_on_invariant"]:
            raise RuntimeError("load stopped on an application or transport contract failure; inspect raw samples")
        report["publication_drain"] = drain_publication(path, profiles, args.nodes)
        if not report["publication_drain"]["drained"]:
            raise RuntimeError("publication backlog did not drain; inspect retained backlog samples")
        traces_path.mkdir()
        trace_events = []
        for name in sorted(expected_nodes):
            log = compose(path, profiles, "logs", "--no-color", "--no-log-prefix", "--since", report["started_at"], name)
            (traces_path / f"{name}.log").write_text(log + "\n")
            trace_events.extend(action_traces.parse_log(log, name))
        actions = action_traces.join(samples, trace_events)
        with (traces_path / "actions.jsonl").open("x") as joined:
            for action in actions:
                joined.write(json.dumps(action) + "\n")
        report["action_traces"] = {
            "directory": traces_path.name, "acknowledged_writes": len(actions),
            "proofs": dict(Counter(action["proof"] for action in actions)),
            "execution_owners": dict(Counter(action["owner"] for action in actions)),
            "forwarded_writes": sum(action["entry"] != action["owner"] for action in actions),
        }
        report["acknowledgements_before_recovery"] = verify_acknowledged(gateway, args.nodes, samples)
        after = verify_roots(path, profiles, before, acknowledged)
        report["roots_advanced"] = bool(after)
        report["roots_before"] = {cell: value["root"] for cell, value in before.items()}
        report["roots_after"] = {cell: value["root"] for cell, value in after.items()}
        if acknowledged:
            owners_at_recovery, recovery_baseline = owner_map(path, profiles, args.nodes, args.cells)
            target = max(acknowledged)
            report["owner_loss"] = recover_owner(
                path, profiles, gateway, args.nodes, owners_at_recovery[target],
                recovery_baseline[target], acknowledged[target], target, samples,
            )
        report["passed"] = (balanced and not report["node_observations"]["errors"]
                            and summary["outcomes"].get("success", 0) == summary["planned_pairs"]
                            and bool(after))
        if not report["passed"]:
            raise RuntimeError("offered load was not fully served or ingress was uneven; inspect the retained report")
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        report["error"] = str(error)
        raise
    finally:
        output.write_text(json.dumps(report, indent=2) + "\n")
        print(output)


if __name__ == "__main__":
    main()
