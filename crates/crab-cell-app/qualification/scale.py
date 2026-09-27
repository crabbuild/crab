#!/usr/bin/env python3
"""Run the built reference application on 3/5/10/20 constrained Compose nodes."""

from __future__ import annotations

import argparse
import copy
import json
import os
from pathlib import Path
import re
import subprocess
import time


class Fleet:
    def __init__(self, state: Path, project: str, overrides: list[Path]):
        self.state = state.resolve(strict=True)
        self.project = project
        if not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,55}", project):
            raise ValueError("use a short, unique lowercase Compose project name")
        self.env = dict(os.environ, CRAB_REFERENCE_STATE=str(self.state))
        existing = self.run("docker", "ps", "-aq", "--filter", f"label=com.docker.compose.project={project}")
        if existing.strip():
            raise ValueError("project already has containers; retain it and choose a fresh project")
        self.evidence = self.state / "evidence" / "scaling"
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
        config["services"]["driver"]["command"] = ["scale"]
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
        assert len(self.active) == 20 and len(self.killed) == 1
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
        killed = next(iter(self.killed))
        driver_log = (self.evidence / "driver.log").read_text()
        for nodes, readers in [(3, 2), (5, 3), (10, 9), (20, 19)]:
            assert f"PERF reader_scale: nodes={nodes} readers={readers} " in driver_log
        assert f"killed_node={killed} ready_readers=3 exact_queries=12 " in driver_log
        source = (self.state / "evidence/source-revision.txt").read_text().strip()
        result = dict(verified=True, source=source, binary_sha256=binaries.pop(),
                      roles=reports, killed_node=killed, events=self.events)
        (self.evidence / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"Verified 3/5/10/20 nodes and reader replacement; evidence: {self.evidence}", flush=True)

    def retain(self) -> None:
        (self.evidence / "compose.log").write_text(self.compose("logs", "--no-color"))
        containers = self.compose("ps", "-aq").split()
        if containers:
            (self.evidence / "containers.json").write_text(self.run("docker", "inspect", *containers))
        self.compose("stop")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True, help="prepared source, binary and evidence directory")
    parser.add_argument("--project", required=True, help="fresh Compose project; stopped containers are retained")
    parser.add_argument("--compose-file", type=Path, action="append", default=[], help="explicit image/cache override")
    args = parser.parse_args()
    fleet = Fleet(args.state, args.project, args.compose_file)
    try:
        fleet.execute()
    finally:
        fleet.retain()


if __name__ == "__main__":
    main()
