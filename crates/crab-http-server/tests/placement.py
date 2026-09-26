#!/usr/bin/env python3
"""Compare signed placement with live capacity and runtime observations."""

import argparse
import json
import re
import subprocess
import sys
import time
from decimal import Decimal, InvalidOperation


def metric(metrics, name):
    values = re.findall(rf"^{re.escape(name)} (\S+)$", metrics, re.MULTILINE)
    if len(values) != 1:
        raise ValueError(f"expected one {name} sample")
    try:
        value = Decimal(values[0])
    except InvalidOperation as error:
        raise ValueError(f"invalid {name} sample") from error
    if not value.is_finite() or value < 0 or value != value.to_integral_value():
        raise ValueError(f"invalid {name} count")
    return int(value)


def placement_match(capacity, session, observation):
    node = observation["node"]
    before, after = observation["metrics_before"], observation["metrics"]
    if observation["process_before"] != observation["process"]:
        raise ValueError("node restarted while observing placement")
    if node["session"] != session or node["live"] is not True:
        raise ValueError("placement does not belong to the expected live session")
    placement = node["advertisement"]["placement"]
    expected = {
        "memory_capacity_bytes": capacity["resources"]["memory_bytes"],
        "disk_capacity_bytes": capacity["admission"]["local_disk_bytes"],
        "max_active_cells": capacity["admission"]["active_cells"],
    }
    if any(placement.get(key) != value for key, value in expected.items()):
        raise ValueError(f"advertised capacity differs from live admission: {expected} != {placement}")
    for metrics in (before, after):
        for name, value in (
            ("local_disk_capacity_bytes", expected["disk_capacity_bytes"]),
            ("active_cell_capacity", expected["max_active_cells"]),
        ):
            if metric(metrics, "crab_http_server_cell_runtime_" + name) != value:
                raise ValueError(f"runtime {name} differs from live admission")
    active_before = metric(before, "crab_http_server_cell_runtime_active_cells")
    active_after = metric(after, "crab_http_server_cell_runtime_active_cells")
    generation = node["advertisement"]["generation"]
    if type(generation) is not int or generation < 1:
        raise ValueError("invalid advertisement generation")
    matched = active_before == active_after == placement["active_cells"]
    return generation, active_before, active_after, matched


def collect_placement(capacity, session, observe, clock=time.monotonic, pause=time.sleep):
    started = clock()
    process = None
    previous_match = None
    previous_generation = 0
    samples = []
    try:
        while clock() - started < 30:
            observation = observe()
            generation, before, after, matched = placement_match(capacity, session, observation)
            if process is not None and observation["process"] != process:
                raise ValueError("node restarted between placement observations")
            process = observation["process"]
            if generation < previous_generation:
                raise ValueError("advertisement generation regressed")
            previous_generation = generation
            elapsed = clock() - started
            samples.append({"elapsed_seconds": elapsed, "generation": generation,
                            "metrics_before": before, "metrics_after": after,
                            "advertised": observation["node"]["advertisement"]["placement"]["active_cells"]})
            if elapsed >= 30:
                break
            # A heartbeat is asynchronous. Require the same bracketed count at
            # two different generations; an unchanged stale advertisement cannot
            # satisfy this, and a transition invalidates the earlier match.
            current = (generation, after) if matched else None
            if (current is not None and previous_match is not None
                    and generation > previous_match[0] and after == previous_match[1]):
                return observation
            previous_match = current
            pause(0.5)
        raise ValueError("placement active-Cell count did not converge within 30 seconds")
    finally:
        print(json.dumps({"placement_session": session, "process": process, "samples": samples}), file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compose", action="append", required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--service", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--capacity", required=True)
    args = parser.parse_args()
    deadline = time.monotonic() + 30
    prefix = ["docker", "compose", "--project-name", args.project]
    for path in args.compose:
        prefix.extend(["--file", path])
    cell = [*prefix, "exec", "-T", args.service, "crab-http-server", "--config", "/etc/crab/server.toml", "cells"]

    def read(*command):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ValueError("placement observation deadline expired")
        return subprocess.run(command, check=True, capture_output=True, text=True, timeout=min(10, remaining)).stdout.strip()

    def process():
        containers = read("docker", "container", "ls", "--all",
                          "--filter", f"label=com.docker.compose.project={args.project}",
                          "--filter", f"label=com.docker.compose.service={args.service}",
                          "--format", "{{.ID}}").splitlines()
        if len(containers) != 1:
            raise ValueError("placement requires exactly one node container")
        return read("docker", "inspect", "--format", "{{.Id}}/{{.State.StartedAt}}", containers[0])

    def observe():
        before = process()
        metrics_before = read(*cell, "metrics")
        node = json.loads(read(*cell, "node", "--session", args.session, "--json"))
        metrics = read(*cell, "metrics")
        return {"process_before": before, "metrics_before": metrics_before,
                "node": node, "metrics": metrics, "process": process()}

    try:
        print(json.dumps(collect_placement(json.loads(args.capacity), args.session, observe)))
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        parser.exit(1, f"{args.service} placement qualification failed: {error}\n")


if __name__ == "__main__":
    main()
