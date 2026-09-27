#!/usr/bin/env python3
"""Run the built reference application on 3/5/10/20 constrained Compose nodes."""

from __future__ import annotations

import argparse
import bisect
import copy
import csv
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time

from entities import verify_entities


def verify_mixed_load(control: Path, nodes: int, label: str = "mixed") -> dict:
    def rows(name):
        path = control / name
        with path.open(newline="") as source:
            result = list(csv.DictReader(source, delimiter="\t"))
        hashes[name] = hashlib.sha256(path.read_bytes()).hexdigest()
        return result

    hashes = {}
    writes = rows(f"{label}-{nodes}-writes.tsv")
    baseline, *arrivals = writes
    assert baseline["outcome"] == baseline["arrival"] == "baseline"
    assert [int(row["arrival"]) for row in arrivals] == list(range(300))
    positions = [int(baseline["sequence"])]
    counts = [int(baseline["count"])]
    missed = 0
    for index, row in enumerate(arrivals):
        assert int(row["scheduled_us"]) == index * 200_000
        delay = int(row["started_us"]) - int(row["scheduled_us"])
        assert delay >= 0
        if row["outcome"] == "scheduler_late":
            assert delay >= 200_000 and row["sequence"] == row["count"] == "0"
            missed += 1
            continue
        assert row["outcome"] == "committed" and delay < 200_000
        assert int(row["sequence"]) > positions[-1]
        assert int(row["count"]) == counts[-1] + 1
        positions.append(int(row["sequence"]))
        counts.append(int(row["count"]))
    assert len(positions) > 1
    reads, behind, lag = 0, 0, 0
    lanes = []
    for lane in range(8):
        samples = rows(f"{label}-{nodes}-reader-{lane}.tsv")
        assert samples and max(int(row["started_us"]) + int(row["elapsed_us"]) for row in samples) >= 59_000_000
        successes = 0
        for row in samples:
            assert 0 <= int(row["started_us"]) < 60_000_000 and int(row["elapsed_us"]) >= 0
            minimum = int(row["minimum_sequence"])
            assert (minimum > 0) == (lane % 2 == 0)
            assert counts[0] <= int(row["latest_count"]) <= counts[-1]
            if minimum:
                assert minimum == positions[int(row["latest_count"]) - counts[0]]
            if row["outcome"] == "behind":
                assert minimum > 0 and row["sequence"] == row["count"] == "0"
                behind += 1
                continue
            assert row["outcome"] == "ok"
            sequence = int(row["sequence"])
            assert max(minimum, positions[0]) <= sequence <= positions[-1]
            index = bisect.bisect_right(positions, sequence) - 1
            assert int(row["count"]) == counts[index], "snapshot value disagrees with its receipt"
            lag = max(lag, int(row["latest_count"]) - int(row["count"]))
            successes += 1
        assert successes > 0
        reads += successes
        lanes.append(successes)
    return dict(nodes=nodes, window_seconds=60, planned_writes=300,
                acknowledged_writes=len(positions) - 1, missed_writes=missed,
                fully_served_writes=missed == 0, successful_reads=reads,
                behind_responses=behind, max_acknowledged_count_lag=lag,
                successful_reads_by_lane=lanes, raw_sha256=hashes)


def verify_reader_loss(control: Path, killed_node: int) -> dict:
    result = verify_mixed_load(control, 5, "reader_loss")
    fault_path = control / "reader-loss.tsv"
    with fault_path.open(newline="") as source:
        events = list(csv.DictReader(source, delimiter="\t"))
    assert len(events) == 1
    event = {key: int(value) for key, value in events[0].items()}
    assert event["killed_node"] == killed_node
    requested, killed, ready, served = [event[key] for key in
                                        ("requested_us", "killed_us", "ready_us", "served_us")]
    assert 10_000_000 <= requested <= killed <= ready <= served < 50_000_000
    result["raw_sha256"][fault_path.name] = hashlib.sha256(fault_path.read_bytes()).hexdigest()
    result["fault"] = event
    result["phases"] = {}
    # Count calls wholly inside a phase. A pre-fault request completed after
    # recovery must not masquerade as service while the reader was unavailable.
    for phase, low, high in [("before", 0, requested), ("replacement", killed, served),
                             ("after", served, 60_000_000)]:
        timings = {"writes": [], "reads": []}
        lanes = []
        for suffix in ["writes", *[f"reader-{lane}" for lane in range(8)]]:
            kind = "writes" if suffix == "writes" else "reads"
            with (control / f"reader_loss-5-{suffix}.tsv").open(newline="") as source:
                samples = list(csv.DictReader(source, delimiter="\t"))
            elapsed = [int(row["elapsed_us"]) for row in samples
                       if row["outcome"] in ("committed", "ok")
                       and low <= int(row["started_us"])
                       and int(row["started_us"]) + int(row["elapsed_us"]) <= high]
            assert elapsed, f"{suffix} made no progress {phase} reader replacement"
            timings[kind].extend(elapsed)
            if kind == "reads":
                lanes.append(len(elapsed))
        summary = {"read_successes_by_lane": lanes}
        for kind, elapsed in timings.items():
            ordered = sorted(elapsed)
            summary[kind] = dict(successes=len(elapsed),
                                 p99_ms=ordered[(len(ordered) * 99 + 99) // 100 - 1] / 1000,
                                 max_ms=ordered[-1] / 1000)
        result["phases"][phase] = summary
    return result


class Fleet:
    def __init__(self, state: Path, project: str, overrides: list[Path], workload: str = "readers"):
        assert workload in ("readers", "entities")
        self.workload = workload
        self.state = state.resolve(strict=True)
        self.project = project
        if not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,55}", project):
            raise ValueError("use a short, unique lowercase Compose project name")
        self.env = dict(os.environ, CRAB_REFERENCE_STATE=str(self.state))
        existing = self.run("docker", "ps", "-aq", "--filter", f"label=com.docker.compose.project={project}")
        if existing.strip():
            raise ValueError("project already has containers; retain it and choose a fresh project")
        self.evidence = self.state / "evidence" / ("scaling" if workload == "readers" else "entity-scaling")
        self.evidence.mkdir(mode=0o1777)
        self.evidence.chmod(0o1777)
        self.control = self.evidence / "control"
        self.control.mkdir(mode=0o1777)
        self.control.chmod(0o1777)
        source = self.state / "source/crates/crab-cell-app/qualification/compose.yaml"
        command = ["docker", "compose", "-p", project, "-f", str(source)]
        for override in overrides:
            command += ["-f", str(override.resolve(strict=True))]
        config = json.loads(self.run(*command, "config", "--format", "json"))
        node = config["services"]["node-0"]
        # Twenty live nodes plus one killed boot. New boots have new identities;
        # restarting a deterministic fixture session would bypass expiry fencing.
        for index in range(3, 21):
            added = copy.deepcopy(node)
            added["environment"].update(
                CRAB_CELL_PERF_PROCESS_NODE=str(index),
                CRAB_CELL_PERF_PROCESS_ADVERTISE=f"node-{index}:8080",
            )
            config["services"][f"node-{index}"] = added
        config["services"]["driver"]["command"] = ["scale" if workload == "readers" else "entity-scale"]
        if workload == "entities":
            for name, service in config["services"].items():
                if name.startswith("node-"):
                    service["command"] = ["entity-node"]
        for service in config["services"].values():
            for volume in service.get("volumes", []):
                if volume["target"] == "/evidence":
                    volume["source"] = str(self.evidence)
        self.compose_file = self.evidence / "compose.json"
        self.compose_file.write_text(json.dumps(config, indent=2) + "\n")
        self.compose_command = ["docker", "compose", "-p", project, "-f", str(self.compose_file)]
        self.active: dict[int, str] = {}
        self.started = 0
        self.killed: dict[int, str] = {}
        self.events: list[dict] = []
        self.driver = ""

    def run(self, *command: str, timeout: int = 180) -> str:
        result = subprocess.run(command, env=self.env, text=True, capture_output=True, timeout=timeout)
        if result.returncode:
            raise RuntimeError(f"{' '.join(command[:4])} failed: {result.stderr[-4000:]}")
        return result.stdout

    def compose(self, *args: str, timeout: int = 180) -> str:
        return self.run(*self.compose_command, *args, timeout=timeout)

    def inspect(self, container: str) -> dict:
        return json.loads(self.run("docker", "inspect", container, timeout=15))[0]

    def limits(self, container: str) -> dict:
        observed = self.inspect(container)
        config = observed["HostConfig"]
        assert config["NanoCpus"] == 1_000_000_000
        assert config["Memory"] == config["MemorySwap"] == 1_073_741_824
        assert config["PidsLimit"] == 256 and config["ReadonlyRootfs"]
        assert "ALL" in config["CapDrop"]
        assert not observed["State"]["OOMKilled"]
        for mount in observed["Mounts"]:
            if mount["Destination"] in ("/source", "/target"):
                assert not mount["RW"]
        return observed

    def scale(self, count: int) -> None:
        assert count in (3, 5, 10, 20) and count > len(self.active)
        added = list(range(self.started, self.started + count - len(self.active)))
        assert added[-1] <= 20
        # The driver's completed dependency chain already created the bucket.
        self.compose("up", "-d", "--no-deps", *[f"node-{node}" for node in added])
        for node in added:
            container = self.compose("ps", "-aq", f"node-{node}").strip()
            assert container
            self.limits(container)
            self.active[node] = container
        self.started += len(added)

    def kill(self, node: int) -> None:
        assert node >= 3 and node in self.active and not self.killed
        container = self.active[node]
        assert self.limits(container)["State"]["Running"]
        counters = self.run(
            "docker", "exec", container, "sh", "-c",
            'for f in cpu.max cpu.stat memory.max memory.swap.max memory.peak memory.events; '
            'do echo "$f"; cat "/sys/fs/cgroup/$f"; done',
            timeout=15,
        )
        (self.evidence / f"node-{node}-kernel-before-kill.txt").write_text(counters)
        self.run("docker", "kill", "--signal", "KILL", container, timeout=15)
        assert self.run("docker", "wait", container, timeout=15).strip() == "137"
        state = self.limits(container)["State"]
        assert not state["Running"] and state["ExitCode"] == 137
        self.killed[node] = self.active.pop(node)

    def execute(self) -> None:
        self.driver = self.compose("run", "-d", "--name", f"{self.project}-driver", "driver").strip()
        self.limits(self.driver)
        sequence = 0
        deadline = time.monotonic() + 20 * 60
        while True:
            containers = json.loads(self.run("docker", "inspect", self.driver, *self.active.values(), timeout=20))
            if not containers[0]["State"]["Running"]:
                assert containers[0]["State"]["ExitCode"] == 0, "driver failed; inspect driver.log"
                break
            assert time.monotonic() < deadline, "scaling driver exceeded 20 minutes"
            for node, observed in zip(self.active, containers[1:], strict=True):
                state = observed["State"]
                assert state["Running"] or (
                    (self.control / "stop").exists() and state["ExitCode"] == 0
                ), f"node {node} exited unexpectedly: {state}"
            request = self.control / f"fleet-{sequence}.request"
            if request.exists():
                action, argument = request.read_text().split()
                count = int(argument)
                started = time.time()
                if action == "scale":
                    self.scale(count)
                elif action == "kill":
                    self.kill(count)
                else:
                    raise ValueError(f"unknown controller command: {action}")
                event = dict(sequence=sequence, action=action, argument=count,
                             started_at=started, completed_at=time.time(), active_nodes=sorted(self.active))
                self.events.append(event)
                (self.evidence / "events.json").write_text(json.dumps(self.events, indent=2) + "\n")
                temporary = request.with_suffix(".tmp")
                temporary.write_text(" ".join(map(str, sorted(self.active))))
                temporary.replace(request.with_suffix(".done"))
                print(json.dumps(event), flush=True)
                sequence += 1
            time.sleep(0.5)
        self.verify()

    def verify(self) -> None:
        assert len(self.active) == 20 and len(self.killed) == (1 if self.workload == "readers" else 0)
        assert [(event["action"], event["argument"]) for event in self.events if event["action"] == "scale"] == [
            ("scale", 3), ("scale", 5), ("scale", 10), ("scale", 20)
        ]
        roles = {f"node-{node}": container for node, container in self.active.items()}
        roles["driver"] = self.driver
        reports = {}
        volumes = set()
        binaries = set()
        for role, container in roles.items():
            assert self.run("docker", "wait", container, timeout=30).strip() == "0", role
            observed = self.limits(container)
            mounts = [mount["Name"] for mount in observed["Mounts"] if mount["Destination"] == "/scratch"]
            assert len(mounts) == 1 and mounts[0] not in volumes
            volumes.update(mounts)
            log = (self.evidence / f"{role}.log").read_text()
            assert "test result: ok. 1 passed; 0 failed;" in log, role
            if role != "driver":
                node = role.split("-")[1]
                assert re.search(rf"node_{node}_session_withdrawn: generation=\d+", log)
                if self.workload == "readers":
                    assert f"node_{node}_reader_drained: activation_closed=1 resolver_closed=1" in log
            counters = (self.evidence / f"{role}-kernel-after.txt").read_text()
            assert "cpu.max\n100000 100000\n" in counters
            assert "memory.max\n1073741824\n" in counters
            assert "memory.swap.max\n0\n" in counters
            assert re.search(r"^oom 0$", counters, re.M) and re.search(r"^oom_kill 0$", counters, re.M)
            peak = int(re.search(r"memory.peak\n(\d+)", counters)[1])
            reports[role] = dict(exit_code=observed["State"]["ExitCode"], memory_peak_bytes=peak)
            binaries.add((self.evidence / f"{role}-binary.sha256").read_text().split()[0])
        assert len(binaries) == 1
        source = (self.state / "evidence/source-revision.txt").read_text().strip()
        if self.workload == "entities":
            result = dict(workload=self.workload, source=source, binary_sha256=binaries.pop(),
                          roles=reports, events=self.events, **verify_entities(self.control))
            (self.evidence / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
            print(f"Verified entity integrity and resources at 3/5/10/20 nodes; evidence: {self.evidence}", flush=True)
            return
        killed = next(iter(self.killed))
        driver_log = (self.evidence / "driver.log").read_text()
        for nodes, readers in [(3, 2), (5, 3), (10, 9), (20, 19)]:
            assert f"PERF reader_scale: nodes={nodes} readers={readers} " in driver_log
        mixed = [verify_mixed_load(self.control, nodes) for nodes in (3, 5, 10, 20)]
        assert f"killed_node={killed} ready_readers=3 exact_queries=12 " in driver_log
        result = dict(verified=True, source=source, binary_sha256=binaries.pop(),
                      roles=reports, killed_node=killed, events=self.events, mixed_load=mixed,
                      reader_loss_load=verify_reader_loss(self.control, killed))
        (self.evidence / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"Verified 3/5/10/20 nodes and reader replacement; evidence: {self.evidence}", flush=True)

    def retain(self) -> None:
        try:
            (self.evidence / "compose.log").write_text(self.compose("logs", "--no-color"))
            containers = self.compose("ps", "-aq").split()
            if containers:
                (self.evidence / "containers.json").write_text(self.run("docker", "inspect", *containers))
            rustfs = self.compose("ps", "-aq", "rustfs").strip()
            if rustfs:
                self.run("docker", "cp", f"{rustfs}:/data/logs", str(self.evidence / "rustfs-logs"))
        finally:
            self.compose("stop")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True, help="prepared source, binary and evidence directory")
    parser.add_argument("--project", required=True, help="fresh Compose project; stopped containers are retained")
    parser.add_argument("--compose-file", type=Path, action="append", default=[], help="explicit image/cache override")
    parser.add_argument("--workload", choices=("readers", "entities"), default="readers", help="reader replacement or scheduled writable entity traffic")
    args = parser.parse_args()
    fleet = Fleet(args.state, args.project, args.compose_file, args.workload)
    try:
        fleet.execute()
    finally:
        fleet.retain()


if __name__ == "__main__":
    main()
