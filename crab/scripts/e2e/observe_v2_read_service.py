#!/usr/bin/env python3
"""Observe owned native read-service processes while a qualification command runs.

The owner starts/stops replicas and supplies a private observation profile. This
tool verifies process/binary identity, samples actual RSS and private metrics,
meters the selected provider proxy and waits for admitted work to drain. It
does not impose quotas, reset caches, publish data, or confer hosted support.
"""
from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
import shlex
import ssl
import statistics
import subprocess
import threading
import time
import urllib.request
from urllib.parse import urlsplit

from qualify_v2_read_service import canonical_hash, endpoint, require, sha256
from run_concurrent_push_smoke import RequestCountingProxy

METRICS = (
    "crab_git_cache_bytes", "crab_git_resident_read_bytes",
    "crab_git_resident_read_capacity_bytes", "crab_git_active_cpu_jobs",
    "crab_git_active_blame_producers", "crab_git_active_blame_waiters",
    "crab_git_active_origin_operations", "crab_git_blame_producers_total",
    "crab_git_cache_verification_bytes_total", "crab_server_admission_available_permits",
    "crab_server_active_sized_response_bytes", "crab_cellar_rss_bytes",
)
DRAIN = ("crab_git_active_cpu_jobs", "crab_git_active_blame_producers",
         "crab_git_active_blame_waiters", "crab_git_active_origin_operations")
MINIMUM_SOAK_SECONDS = 600


def metrics_values(text: str) -> dict[str, float]:
    values = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        name, _, amount = line.partition(" ")
        if name.split("{", 1)[0] not in METRICS:
            continue
        value = float(amount)
        require(math.isfinite(value) and value >= 0, "invalid observed metric")
        values[name] = value
    require(bool(values), "missing read service metrics")
    return values


def process_identity(pid: int, binary: Path) -> str:
    require(type(pid) is int and pid > 1, "invalid replica PID")
    command = subprocess.check_output(["ps", "-o", "command=", "-p", str(pid)], text=True).strip()
    require(Path(shlex.split(command)[0]).resolve() == binary.resolve(), "replica PID uses a different binary")
    started = subprocess.check_output(["ps", "-o", "lstart=", "-p", str(pid)], text=True).strip()
    require(bool(started), "missing process start identity")
    return f"{pid}:{started}"


def resource_sample(pid: int) -> dict:
    fields = subprocess.check_output(["ps", "-o", "rss=,pcpu=", "-p", str(pid)], text=True).split()
    require(len(fields) == 2, "replica exited during resource sampling")
    return {"rss_bytes":int(fields[0]) * 1024, "ps_cpu_percent":float(fields[1])}


def resource_gates(samples: list[dict], profile: dict) -> list[str]:
    require(len(samples) >= 2, "insufficient resource samples")
    failures = []
    for index, replica in enumerate(profile["replicas"]):
        rss = [sample["replicas"][index]["rss_bytes"] for sample in samples]
        if max(rss) > replica["maximum_rss_bytes"]:
            failures.append(f"replica-{index}:rss_envelope")
        # Startup allocates caches and native Cells. Only a complete soak can
        # qualify a steady-state slope; short functional runs retain RSS peaks
        # and ownership checks without pretending to prove a plateau.
        if len(rss) >= 10 and samples[-1]["elapsed_seconds"] - samples[0]["elapsed_seconds"] >= MINIMUM_SOAK_SECONDS:
            window = len(rss) // 5
            previous, late = statistics.median(rss[-2 * window:-window]), statistics.median(rss[-window:])
            if late > previous * profile["maximum_late_rss_ratio"]:
                failures.append(f"replica-{index}:rss_growth")
        for sample in samples:
            values = sample["replicas"][index]["metrics"]
            used, maximum = values.get("crab_git_resident_read_bytes"), values.get("crab_git_resident_read_capacity_bytes")
            if used is not None and maximum is not None and used > maximum:
                failures.append(f"replica-{index}:resident_admission")
                break
    return failures


class Observation:
    def __init__(self, profile: dict, output: Path):
        require(profile.keys() <= {"schema", "build_receipt", "replicas", "tls", "provider_url",
                                  "maximum_late_rss_ratio", "sample_interval_seconds", "quota_enforcement"},
                "unexpected observation profile field")
        require(profile.get("schema") == 1 and 1 <= len(profile["replicas"]) <= 2, "invalid observation profile")
        require(0 < profile["sample_interval_seconds"] <= 10 and profile["maximum_late_rss_ratio"] >= 1,
                "invalid resource observation policy")
        self.profile, self.output = profile, output
        self.binding = json.loads(Path(profile["build_receipt"]).read_text())
        self.binary = Path(self.binding["server_binary"])
        require(sha256(self.binary) == self.binding["server_binary_sha256"], "observed binary differs from receipt")
        self.context = ssl.create_default_context(cafile=profile["tls"]["ca"])
        self.context.load_cert_chain(profile["tls"]["certificate"], profile["tls"]["key"])
        # Provider measurements come only from the explicitly selected local
        # fixture meter; remote provider authentication stays with its owner.
        provider = urlsplit(profile["provider_url"])
        endpoint(f"{provider.scheme}://{provider.netloc}")
        require(provider.hostname in {"localhost", "127.0.0.1", "::1"}, "provider meter must be loopback")
        self.identities = []
        for replica in profile["replicas"]:
            require(replica.keys() == {"pid", "metrics_url", "maximum_rss_bytes"} and replica["maximum_rss_bytes"] > 0,
                    "invalid observed replica")
            require(replica["metrics_url"].startswith("https://"), "private metrics require verified TLS")
            self.identities.append(process_identity(replica["pid"], self.binary))
        self.samples, self.errors = [], []
        self.stopped = threading.Event()
        self.thread = threading.Thread(target=self._sample_loop, daemon=True)

    def fetch(self, url: str) -> bytes:
        with urllib.request.urlopen(url, context=self.context, timeout=5) as response:
            return response.read(4 * 1024 * 1024)

    def sample(self) -> dict:
        replicas = []
        for replica in self.profile["replicas"]:
            values = metrics_values(self.fetch(replica["metrics_url"]).decode())
            replicas.append({**resource_sample(replica["pid"]), "metrics":values})
        sample = {"elapsed_seconds":time.monotonic() - self.started, "replicas":replicas}
        self.samples.append(sample)
        with (self.output / "resources.jsonl").open("a") as journal:
            journal.write(json.dumps(sample, allow_nan=False) + "\n")
        return sample

    def _sample_loop(self):
        while not self.stopped.wait(self.profile["sample_interval_seconds"]):
            try:
                self.sample()
            except Exception as error:
                self.errors.append(type(error).__name__)
                self.stopped.set()

    def start(self):
        self.output.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.started = time.monotonic()
        self.before = json.loads(self.fetch(self.profile["provider_url"]))
        self.sample()
        self.thread.start()

    def finish(self) -> dict:
        self.stopped.set()
        self.thread.join()
        deadline = time.monotonic() + 30
        last, present, drained = None, False, False
        while True:
            try:
                last = self.sample()
            except Exception as error:
                self.errors.append("final_sample:" + type(error).__name__)
                break
            present = all(all(name in replica["metrics"] for name in DRAIN) for replica in last["replicas"])
            drained = present and all(replica["metrics"][name] == 0 for replica in last["replicas"] for name in DRAIN)
            if drained or not present or time.monotonic() >= deadline:
                break
            time.sleep(.1)
        delta = None
        try:
            after = json.loads(self.fetch(self.profile["provider_url"]))
            delta = RequestCountingProxy.delta(self.before, after)
        except Exception as error:
            self.errors.append("final_provider:" + type(error).__name__)
        failures = resource_gates(self.samples, self.profile) if len(self.samples) >= 2 else ["insufficient_resource_samples"]
        failures += ["sample:" + error for error in self.errors]
        for index, replica in enumerate(self.profile["replicas"]):
            try:
                require(process_identity(replica["pid"], self.binary) == self.identities[index], "observed replica restarted")
            except Exception as error:
                failures.append(f"replica-{index}:identity:" + type(error).__name__)
        if present and not drained:
            failures.append("admitted_work_did_not_drain")
        if delta is not None and delta["proxy_errors"]:
            failures.append("provider_proxy_errors")
        report = {"schema":1, "status":"failed" if failures else "observed", "failures":failures,
                  "profile_sha256":canonical_hash(self.profile),
                  "build_receipt_sha256":sha256(Path(self.profile["build_receipt"])),
                  "observer_sha256":sha256(Path(__file__)), "process_start_identities":self.identities,
                  "provider_counter_sha256":sha256(Path(__file__).with_name("run_concurrent_push_smoke.py")),
                  "samples":len(self.samples), "duration_seconds":time.monotonic() - self.started,
                  "peak_rss_bytes":[max((sample["replicas"][i]["rss_bytes"] for sample in self.samples), default=None)
                                    for i in range(len(self.identities))],
                  "last":last, "admitted_work_drained":drained if present else None,
                  "rss_plateau":"insufficient_duration" if len(self.samples) < 2 or self.samples[-1]["elapsed_seconds"] - self.samples[0]["elapsed_seconds"] < MINIMUM_SOAK_SECONDS
                                else "failed" if any(name.endswith(":rss_growth") for name in failures) else "passed",
                  "provider":delta, "quota_enforcement":self.profile["quota_enforcement"],
                  "limits":{key:self.profile[key] for key in ("maximum_late_rss_ratio", "sample_interval_seconds")}}
        (self.output / "report.json").write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
        return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    require(bool(command), "missing measured command")
    observation = Observation(json.loads(args.profile.read_text()), args.output)
    observation.start()
    result = None
    try:
        result = subprocess.run(command, check=False)
    finally:
        report = observation.finish()
        report["command_exit_code"] = result.returncode if result is not None else None
        if result is None or result.returncode:
            report["failures"].append("measured_command_failed")
            report["status"] = "failed"
        (args.output / "report.json").write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
    return result.returncode or int(bool(report["failures"]))


if __name__ == "__main__":
    raise SystemExit(main())
