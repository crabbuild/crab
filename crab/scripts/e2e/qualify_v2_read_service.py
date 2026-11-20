#!/usr/bin/env python3
"""Measure real repository HTTP reads against an independent, read-only Git oracle.

Run in an owned qualification environment. Process restarts, cache preparation,
faults and resource sampling belong to its orchestrator; this driver never
resets a production cache or treats a repeated request as a cold sample.
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client
import ipaddress
import json
import math
import os
from pathlib import Path
import re
import socket
import stat
import subprocess
import threading
import time
from urllib.parse import quote, urlencode, urlsplit

from verify_remote_browse import require
from read_service_samples import percentiles, read_journal, summarize

SCHEMA = 1
ACTIONS = {"file", "file-preview", "blob", "commits", "blame"}
CACHE_STATES = {"cold", "warm_objects", "warm_comparisons", "warm_result",
                "post_push", "force_push", "restart"}
MAX_JSON_BYTES = 8 * 1024 * 1024


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_hash(value) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     allow_nan=False).encode()).hexdigest()


def endpoint(value: str):
    require(isinstance(value, str), "service URL must be text")
    parsed = urlsplit(value)
    require(parsed.scheme in {"http", "https"} and parsed.hostname is not None
            and not parsed.username and not parsed.password and not parsed.query
            and not parsed.fragment and parsed.path in {"", "/"}, "invalid service URL")
    try:
        loopback = ipaddress.ip_address(parsed.hostname).is_loopback
    except ValueError:
        loopback = parsed.hostname == "localhost"
    require(parsed.scheme == "https" or loopback, "HTTP requires a loopback fixture")
    return parsed, loopback


def load_headers(path: Path | None, urls: list[str]) -> dict:
    for url in urls:
        endpoint(url)
    if path is None:
        require(all(endpoint(url)[1] for url in urls), "hosted qualification requires a headers file")
        return {}
    details = path.lstat()
    require(stat.S_ISREG(details.st_mode) and details.st_uid == os.getuid()
            and not details.st_mode & 0o077 and details.st_size <= 16384,
            "headers file must be an owner-private regular file")
    headers = json.loads(path.read_text())
    require(isinstance(headers, dict) and bool(headers), "headers file must contain HTTP headers")
    require(all(isinstance(key, str) and isinstance(value, str)
                and key.lower() in {"authorization", "cookie"}
                and not any(char in value for char in "\r\n")
                for key, value in headers.items()), "invalid authentication headers")
    return headers


def validate_profile(profile: dict) -> None:
    require(isinstance(profile, dict) and profile.keys() <= {
        "schema", "source_repository", "build_receipt", "provider", "region", "resources",
        "workloads", "cases", "thresholds", "targets", "replica_urls", "baseline_report", "public_origin",
    }, "unexpected qualification profile field")
    require(profile.get("schema") == SCHEMA, "unsupported profile schema")
    if profile.get("public_origin") is not None:
        endpoint(profile["public_origin"])
    for name in ("source_repository", "build_receipt", "provider", "region"):
        require(isinstance(profile.get(name), str) and bool(profile[name]), "missing profile identity")
    resources = profile.get("resources", {})
    require(isinstance(resources, dict) and resources.keys() <= {"cpu", "memory_bytes", "disk_bytes"},
            "unexpected resource envelope field")
    require(all(isinstance(resources.get(key), (int, float)) and resources[key] > 0
                and math.isfinite(resources[key]) for key in ("cpu", "memory_bytes", "disk_bytes")),
            "missing resource envelope")
    workloads = profile.get("workloads", [])
    require(bool(workloads), "missing workloads")
    names = [item["name"] for item in workloads]
    require(len(names) == len(set(names)), "duplicate workload name")
    for item in workloads:
        require(item.keys() <= {"name", "action", "class", "paths", "page_size", "page_depth", "cursor_receipt", "baseline_action", "baseline_cursor_receipt"},
                "unexpected workload field")
        require(item.get("action") in ACTIONS and item.get("class") in {"interactive", "bulk"},
                "invalid workload action or class")
        require(item.get("baseline_action", item["action"]) == item["action"]
                or (item.get("baseline_action"), item["action"]) == ("file", "file-preview"),
                "unsupported baseline/candidate endpoint migration")
        require(isinstance(item.get("paths"), list) and bool(item["paths"])
                and all(isinstance(path, str) and "\0" not in path for path in item["paths"]),
                "missing workload paths")
        require(1 <= item.get("page_size", 100) <= 200 and 1 <= item.get("page_depth", 1) <= 100,
                "invalid history page coordinates")
    cases = profile.get("cases", [])
    require(bool(cases) and len({item["name"] for item in cases}) == len(cases), "missing or duplicate cases")
    for item in cases:
        require(item.keys() <= {"name", "cache", "pattern", "clients", "duration_seconds", "warmup_seconds",
                                "timeout_seconds", "workloads", "state_receipt", "reference_case", "cancel_after_seconds", "maximum_failure_fraction"},
                "unexpected measurement case field")
        require(item.get("cache") in CACHE_STATES and item.get("pattern") in {"same_key", "distinct"},
                "unclassified cache state or request pattern")
        require(isinstance(item.get("clients"), int) and 1 <= item["clients"] <= 64,
                "invalid offered concurrency")
        require(0 < item.get("duration_seconds", 0) <= 3600 and 0 <= item.get("warmup_seconds", -1) <= 600
                and 0 < item.get("timeout_seconds", 0) <= 600, "invalid measurement duration")
        require(bool(item.get("workloads")) and set(item["workloads"]) <= set(names), "unknown case workload")
        require(isinstance(item.get("state_receipt"), str), "missing process/cache state receipt")
        if item.get("cancel_after_seconds") is not None:
            require(0 < item["cancel_after_seconds"] < item["timeout_seconds"], "invalid client cancellation time")
        require(item["cache"] != "cold" or item["warmup_seconds"] == 0,
                "cold initial-state cases cannot run an unreported warm-up")
        if "maximum_failure_fraction" in item:
            require(isinstance(item["maximum_failure_fraction"], (int, float))
                    and 0 <= item["maximum_failure_fraction"] < 1,
                    "invalid case failure envelope")
    thresholds = profile.get("thresholds", {})
    require(thresholds.keys() <= {"minimum_successes", "maximum_failure_fraction", "target_p95_ratio",
                                 "maximum_control_regression_ratio", "maximum_interactive_ratio"},
            "unexpected threshold field")
    for name in ("minimum_successes", "maximum_failure_fraction", "target_p95_ratio",
                 "maximum_control_regression_ratio", "maximum_interactive_ratio"):
        require(isinstance(thresholds.get(name), (int, float)) and math.isfinite(thresholds[name]),
                "missing frozen threshold")
    require(thresholds["minimum_successes"] >= 20
            and 0 <= thresholds["maximum_failure_fraction"] < 1
            and 0 < thresholds["target_p95_ratio"] < 1
            and thresholds["maximum_control_regression_ratio"] >= 1
            and thresholds["maximum_interactive_ratio"] >= 1, "invalid frozen threshold")
    targets = profile.get("targets", [])
    require(bool(targets), "missing target cells")
    require(all(target.keys() <= {"case", "workload"} for target in targets), "unexpected target field")
    by_name = {case["name"]:case for case in cases}
    require(all(target.get("case") in by_name
                and target.get("workload") in by_name[target["case"]]["workloads"] for target in targets),
            "invalid target cell")
    for case in cases:
        reference = case.get("reference_case")
        if reference is not None:
            require(reference in by_name and reference != case["name"], "invalid interactive reference case")
            interactive = {item["name"] for item in workloads if item["class"] == "interactive"}
            require(set(case["workloads"]) & interactive <= set(by_name[reference]["workloads"])
                    and set(by_name[reference]["workloads"]) <= interactive,
                    "interactive reference must contain matching small reads only")


def build_binding(profile: dict) -> dict:
    receipt_path = Path(profile["build_receipt"])
    receipt = json.loads(receipt_path.read_text())
    validate_binding(receipt)
    binary = Path(receipt["server_binary"])
    require(sha256(binary) == receipt["server_binary_sha256"], "server binary differs from build receipt")
    # The environment owns build provenance. Retain the exact receipt, including
    # its source manifest, rather than substituting this driver's checkout SHA.
    require(isinstance(receipt.get("source_inputs"), dict) and bool(receipt["source_inputs"]),
            "missing build source manifest")
    require(all(isinstance(name, str) and re.fullmatch(r"[0-9a-f]{64}", value)
                for name, value in receipt["source_inputs"].items()), "invalid build source manifest")
    fields = ("source_revision", "source_patch_sha256", "server_binary", "server_binary_sha256",
              "compiler", "build_profile", "source_inputs")
    return {**{field:receipt[field] for field in fields}, "receipt_sha256":sha256(receipt_path)}


def validate_binding(receipt: dict) -> None:
    for field in ("source_revision", "source_patch_sha256", "server_binary_sha256", "compiler", "build_profile"):
        require(isinstance(receipt.get(field), str) and bool(receipt[field]), "incomplete source/build receipt")
    require(re.fullmatch(r"[0-9a-f]{40}", receipt["source_revision"]) is not None, "invalid source revision")
    for field in ("source_patch_sha256", "server_binary_sha256"):
        require(re.fullmatch(r"[0-9a-f]{64}", receipt[field]) is not None, "invalid build hash")


class GitOracle:
    def __init__(self, source: Path, revision: str):
        self.source = source
        self.revision = self.git("rev-parse", "--verify", revision + "^{commit}").decode().strip()
        self.expected = {}

    def git(self, *args) -> bytes:
        return subprocess.check_output(["git", "-C", str(self.source), *args], stderr=subprocess.PIPE)

    def prepare(self, workload: dict, path: str) -> None:
        action = workload["action"]
        if action == "commits":
            size, depth = workload.get("page_size", 100), workload.get("page_depth", 1)
            commits = self.git("rev-list", "--first-parent", f"--skip={(depth - 1) * size}",
                               f"--max-count={size + 1}", self.revision).decode().splitlines()
            require(bool(commits), "requested history page does not exist in fixture")
            value = {"commits":commits[:size], "has_more":len(commits) > size}
        else:
            body = self.git("show", self.revision + ":" + path)
            value = {"oid":hashlib.sha1(f"blob {len(body)}\0".encode() + body).hexdigest(),
                     "size":len(body), "sha256":hashlib.sha256(body).hexdigest(), "body":body}
            if action == "blame":
                lines = self.git("blame", "--first-parent", "--root", "--line-porcelain",
                                 self.revision, "--", path).splitlines()
                value["origins"] = [(int(fields[2]), fields[0].decode()) for line in lines
                                    if not line.startswith(b"\t") and len(fields := line.split()) >= 3
                                    and re.fullmatch(b"[0-9a-f]{40}", fields[0]) and fields[2].isdigit()]
        self.expected[(workload["name"], path)] = value

    def check(self, workload: dict, path: str, body: bytes, digest: str, length: int) -> None:
        expected = self.expected[(workload["name"], path)]
        action = workload["action"]
        if action == "blob":
            require(digest == expected["sha256"] and length == expected["size"], "raw content differs from Git")
            return
        value = json.loads(body)
        require(value.get("commit") == self.revision and isinstance(value.get("generation"), int),
                "HTTP snapshot identity differs")
        if action == "commits":
            require([item["oid"] for item in value["items"]] == expected["commits"]
                    and bool(value["next"]) == expected["has_more"], "HTTP history differs from Git")
        elif action == "blame":
            count = len(expected["origins"])
            require(all(isinstance(item["start"], int) and isinstance(item["lines"], int)
                        and item["start"] > 0 and 0 < item["lines"] <= count for item in value["ranges"])
                    and sum(item["lines"] for item in value["ranges"]) == count, "invalid HTTP blame ranges")
            actual = [(line, item["commit"]["oid"]) for item in value["ranges"]
                      for line in range(item["start"], item["start"] + item["lines"])]
            require(actual == expected["origins"], "HTTP blame differs from Git")
        else:
            require(value["oid"] == expected["oid"] and value["size"] == expected["size"],
                    "HTTP file identity differs")
            payload = expected["body"]
            try:
                text = None if b"\0" in payload else payload.decode("utf-8")
            except UnicodeDecodeError:
                text = None
            if action == "file":
                inline = text is not None and len(payload) <= 1024 * 1024
                require(value["text"] == (text if inline else None)
                        and value["text_truncated"] == (text is not None and not inline),
                        "HTTP file representation differs")
            else:
                state = ("omitted_size" if len(payload) > 1024 * 1024 else
                         "binary" if b"\0" in payload else
                         "unsupported_encoding" if text is None else "inline")
                require(value["preview"]["state"] == state, "HTTP preview state differs")
                if state == "inline":
                    require(value["preview"]["text"] == text, "HTTP preview content differs")


class Transport:
    def __init__(self, urls: list[str], headers: dict, timeout: float):
        self.urls, self.headers, self.timeout = urls, headers, timeout
        self.connections = {}

    def close(self):
        for connection in self.connections.values():
            connection.close()
        self.connections.clear()

    def get(self, replica: int, path: str, action: str, cancel_after: float | None = None) -> tuple[dict, bytes]:
        parsed, _ = endpoint(self.urls[replica])
        connection = self.connections.get(replica)
        if connection is None:
            constructor = http.client.HTTPSConnection if parsed.scheme == "https" else http.client.HTTPConnection
            connection = constructor(parsed.hostname, parsed.port, timeout=self.timeout)
            self.connections[replica] = connection
        started = time.perf_counter_ns()
        record = {"status":0, "bytes":0, "ttfb_us":None, "headers_us":None, "error":None}
        body = bytearray()
        digest = hashlib.sha256()
        cancelled = threading.Event()
        timer = None

        def cancel():
            cancelled.set()
            active = connection.sock
            if active is not None:
                try:
                    active.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
            connection.close()

        try:
            connection.request("GET", path, headers={**self.headers, "Accept-Encoding":"identity",
                                                     "Accept":"application/json" if action != "blob" else "*/*"})
            if cancel_after is not None:
                timer = threading.Timer(cancel_after, cancel)
                timer.daemon = True
                timer.start()
            response = connection.getresponse()
            record["status"] = response.status
            record["headers_us"] = (time.perf_counter_ns() - started) / 1000
            record["server_timing_ms"] = server_timings(response.getheader("server-timing", ""))
            # Correlate boundary traces without retaining arbitrary reflected
            # header text, which could contain credentials.
            request_id = response.getheader("x-request-id", "")
            if re.fullmatch(r"[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}", request_id):
                record["request_id"] = request_id
            chunk = response.read(1)
            record["ttfb_us"] = (time.perf_counter_ns() - started) / 1000
            while chunk:
                record["bytes"] += len(chunk)
                digest.update(chunk)
                if action != "blob":
                    require(len(body) + len(chunk) <= MAX_JSON_BYTES, "JSON body exceeds qualification bound")
                    body.extend(chunk)
                chunk = response.read(64 * 1024)
            record["sha256"] = digest.hexdigest()
        except (OSError, http.client.HTTPException, ValueError) as error:
            # Response/error strings may contain reflected credentials. Persist
            # a bounded category only; never log arbitrary headers, URLs or bodies.
            record["error"] = "ClientCancelled" if cancelled.is_set() else type(error).__name__
            connection.close()
            self.connections.pop(replica, None)
        finally:
            if timer is not None:
                timer.cancel()
                timer.join()
            if cancelled.is_set():
                record["error"] = "ClientCancelled"
                self.connections.pop(replica, None)
        record["elapsed_us"] = (time.perf_counter_ns() - started) / 1000
        return record, bytes(body)


def server_timings(value: str) -> dict:
    result = {}
    for entry in value.split(","):
        match = re.fullmatch(r"\s*(open|read|encode|total);dur=([0-9]+(?:\.[0-9]+)?)\s*", entry)
        if match:
            amount = float(match[2])
            require(math.isfinite(amount), "invalid server duration")
            result[match[1]] = amount
    return result


def state_receipt(case: dict, binding: dict) -> dict:
    path = Path(case["state_receipt"])
    receipt = json.loads(path.read_text())
    require(receipt.get("cache") == case["cache"] and bool(receipt.get("replicas")),
            "cache state receipt differs from case")
    for replica in receipt["replicas"]:
        require(bool(replica.get("process_start_identity"))
                and replica.get("server_binary_sha256") == binding["server_binary_sha256"]
                and isinstance(replica.get("cache_counters"), dict)
                and bool(replica["cache_counters"]), "missing process/cache binding")
        require(all(isinstance(value, (int, float)) and math.isfinite(value) and value >= 0
                    for value in replica["cache_counters"].values()), "invalid cache counters")
    fields = ("process_start_identity", "server_binary_sha256", "cache_counters")
    return {"cache":receipt["cache"], "receipt_sha256":sha256(path),
            "replicas":[{field:replica[field] for field in fields} for replica in receipt["replicas"]]}


def run_case(case: dict, workloads: dict, urls: list[str], headers: dict,
             repository: str, oracle: GitOracle, journal) -> float:
    route = "/api/repos/" + "/".join(quote(part, safe="") for part in repository.split("/")) + "/"
    queries = {}
    setup = Transport(urls, headers, case["timeout_seconds"])
    try:
        for name in set(case["workloads"]):
            workload = workloads[name]
            for path in workload["paths"]:
                params = {"rev":oracle.revision}
                if workload["action"] == "commits":
                    params["limit"] = workload.get("page_size", 100)
                    depth = workload.get("page_depth", 1)
                    if depth > 1 and workload.get("cursor_receipt"):
                        prepared = json.loads(Path(workload["cursor_receipt"]).read_text())
                        require(prepared.get("repository") == repository
                                and prepared.get("commit") == oracle.revision
                                and prepared.get("page_size") == params["limit"]
                                and prepared.get("page_depth") == depth
                                and isinstance(prepared.get("cursor"), str) and bool(prepared["cursor"]),
                                "prepared cursor context differs")
                        params["cursor"] = prepared["cursor"]
                    else:
                        require(depth == 1 or case["cache"] not in {"cold", "restart"},
                                "cold deep-page reads require a cursor prepared before process restart")
                        for _ in range(1, depth):
                            record, body = setup.get(0, route + "commits?" + urlencode(params), "commits")
                            record.update(case=case["name"], workload=name, phase="setup")
                            journal.write(json.dumps(record, allow_nan=False) + "\n")
                            require(record["status"] == 200 and record["error"] is None, "history cursor setup failed")
                            cursor = json.loads(body)["next"]
                            require(cursor is not None, "history cursor setup reached EOF")
                            params["cursor"] = cursor
                else:
                    params["path_hex"] = path.encode().hex()
                queries[(name, path)] = route + workload["action"] + "?" + urlencode(params)
    finally:
        setup.close()
    barrier = threading.Barrier(case["clients"])
    lock = threading.Lock()
    started = time.perf_counter()

    def worker(client: int) -> None:
        transport = Transport(urls, headers, case["timeout_seconds"])
        barrier.wait()
        began = time.perf_counter()
        deadline = began + case["warmup_seconds"] + case["duration_seconds"]
        sequence = 0
        try:
            while time.perf_counter() < deadline:
                # Bounded by duration, offered concurrency, and the socket timeout.
                # Each worker has one connection per replica; no queued requests.
                offset = 0 if case["pattern"] == "same_key" else client
                name = case["workloads"][(sequence + offset) % len(case["workloads"])]
                workload = workloads[name]
                path = workload["paths"][(sequence + offset) % len(workload["paths"])]
                replica = (client + sequence) % len(urls)
                phase = "warmup" if time.perf_counter() - began < case["warmup_seconds"] else "measured"
                record, body = transport.get(replica, queries[(name, path)], workload["action"],
                                             case.get("cancel_after_seconds"))
                if record["status"] == 200 and record["error"] is None:
                    try:
                        oracle.check(workload, path, body, record["sha256"], record["bytes"])
                    except (ValueError, KeyError, TypeError, AttributeError, IndexError):
                        record["error"] = "GitOracleMismatch"
                record.update(case=case["name"], workload=name, client=client, sequence=sequence,
                              replica=replica, phase=phase,
                              cache_scope="case_initial_state",
                              initial_burst=sequence == 0)
                with lock:
                    journal.write(json.dumps(record, allow_nan=False) + "\n")
                sequence += 1
        finally:
            transport.close()

    with ThreadPoolExecutor(max_workers=case["clients"]) as workers:
        for _ in workers.map(worker, range(case["clients"])):
            pass
    return time.perf_counter() - started - case["warmup_seconds"]


def validate_report(report: dict, profile: dict) -> list[str]:
    require(report.get("schema") == SCHEMA and bool(report.get("binding")), "missing report/source binding")
    validate_binding(report["binding"])
    expected = {case["name"]:case for case in profile["cases"]}
    actual = {case["name"]:case for case in report["cases"]}
    require(len(report["cases"]) == len(expected) and actual.keys() == expected.keys(), "missing measurement cells")
    failures = []
    for name, case in actual.items():
        require(case["cache"] == expected[name]["cache"], "report cache classification differs")
        require(set(case["workloads"]) == set(expected[name]["workloads"]), "missing workload measurements")
        for summary in case["workloads"].values():
            require(all(type(summary.get(key)) is int and summary[key] >= 0
                        for key in ("attempts", "successes", "errors", "rejected", "oracle_mismatches")), "invalid request counts")
            require(summary["oracle_mismatches"] <= summary["errors"], "oracle mismatch is missing from failed requests")
            if summary["oracle_mismatches"]:
                failures.append(name + ":git_oracle_mismatch")
            require(summary["attempts"] == summary["successes"] + summary["errors"] + summary["rejected"]
                    and summary["attempts"] > 0, "missing failed-request accounting")
            for rate in ("offered_requests_per_second", "successful_requests_per_second"):
                require(isinstance(summary.get(rate), (int, float))
                        and math.isfinite(summary[rate]) and summary[rate] >= 0,
                        "invalid request throughput")
            require(summary["offered_requests_per_second"] >= summary["successful_requests_per_second"],
                    "successful throughput exceeds offered requests")
            if summary["successes"] < profile["thresholds"]["minimum_successes"]:
                failures.append(name + ":insufficient_successes")
            if (summary["errors"] + summary["rejected"]) / summary["attempts"] > expected[name].get("maximum_failure_fraction", profile["thresholds"]["maximum_failure_fraction"]):
                failures.append(name + ":failure_fraction")
            if summary["successes"]:
                require({"p50", "p95"} <= summary["successful_latency_us"].keys()
                        and {"p50", "p95"} <= summary["ttfb_us"].keys(), "missing latency or TTFB")
                require("p99" not in summary["successful_latency_us"] or summary["successes"] >= 1000,
                        "insufficient p99 observations")
                for metric in ("successful_latency_us", "ttfb_us"):
                    values = summary[metric]
                    require(all(isinstance(value, (int, float)) and math.isfinite(value) and value >= 0
                                for value in values.values()) and values["p50"] <= values["p95"],
                            "invalid latency observations")
    interactive = {item["name"] for item in profile["workloads"] if item["class"] == "interactive"}
    for name, case in actual.items():
        reference = expected[name].get("reference_case")
        if reference is not None:
            for workload in set(case["workloads"]) & interactive:
                measured = case["workloads"][workload]
                before = actual[reference]["workloads"][workload]
                if measured["successes"] and before["successes"] and measured["successful_latency_us"]["p95"] > (
                        before["successful_latency_us"]["p95"] * profile["thresholds"]["maximum_interactive_ratio"]):
                    failures.append(name + ":interactive_latency")
    return failures


def workload_identity(profile: dict) -> str:
    value = {key:profile[key] for key in ("resources", "workloads", "targets", "thresholds", "provider", "region")}
    value["public_origin"] = profile.get("public_origin")
    value["cases"] = [{key:value for key, value in case.items() if key != "state_receipt"}
                      for case in profile["cases"]]
    return canonical_hash(value)


def wire_workloads(profile: dict, phase: str) -> dict:
    # Freeze the named /file -> /file-preview migration before either trial;
    # choosing a variant never retries another endpoint after a request fails.
    result = {}
    for item in profile["workloads"]:
        value = {**item, "action":item.get("baseline_action", item["action"]) if phase == "baseline" else item["action"]}
        if phase == "baseline" and item.get("baseline_cursor_receipt"):
            value["cursor_receipt"] = item["baseline_cursor_receipt"]
        result[item["name"]] = value
    return result


def compare_baseline(report: dict, baseline: dict, profile: dict) -> list[str]:
    require(report.get("phase") == "candidate" and baseline.get("phase") == "baseline",
            "comparison requires a baseline and a candidate trial")
    require(report["workload_sha256"] == baseline["workload_sha256"] == workload_identity(profile),
            "baseline workload/resource/threshold profile differs")
    # A baseline may demonstrate the starvation this change is meant to fix.
    # Keep that engineering failure, while rejecting incomplete/error-prone
    # measurements and enforcing the unchanged candidate target separately.
    baseline_failures = validate_report(baseline, profile)
    require(all(failure.endswith(":interactive_latency") for failure in baseline_failures),
            "baseline did not pass measurement gates")
    require(re.fullmatch(r"[0-9a-f]{40}", baseline.get("fixture_commit", "")) is not None
            and report.get("fixture_commit") == baseline["fixture_commit"], "baseline fixture differs")
    require(report["binding"]["compiler"] == baseline["binding"]["compiler"], "baseline compiler differs")
    targets = {(target["case"], target["workload"]) for target in profile["targets"]}
    previous = {case["name"]:case for case in baseline["cases"]}
    failures = []
    for case in report["cases"]:
        for name, summary in case["workloads"].items():
            before = previous[case["name"]]["workloads"][name]
            cell = (case["name"], name)
            if summary["successes"] == 0:
                failures.append(":".join((*cell, "no_successful_requests")))
                continue
            limit = profile["thresholds"]["target_p95_ratio" if cell in targets else "maximum_control_regression_ratio"]
            if summary["successful_latency_us"]["p95"] > before["successful_latency_us"]["p95"] * limit:
                failures.append(":".join((*cell, "p95_regression")))
            if cell not in targets and summary["successful_requests_per_second"] < before["successful_requests_per_second"] * (
                    2 - profile["thresholds"]["maximum_control_regression_ratio"]):
                failures.append(":".join((*cell, "throughput_regression")))
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--text-path", required=True)
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--headers-file", type=Path)
    parser.add_argument("--phase", choices=("baseline", "candidate"), default="candidate")
    args = parser.parse_args()
    profile = json.loads(args.profile.read_text())
    validate_profile(profile)
    require(len(args.repository.split("/")) == 2 and all(args.repository.split("/")), "repository must be owner/name")
    urls = [args.base_url, *profile.get("replica_urls", [])]
    for url in urls:
        endpoint(url)
    headers = load_headers(args.headers_file, urls)
    if profile.get("public_origin") is not None:
        # Connect to each replica directly while retaining the configured public
        # authority. A forwarding proxy would hide disconnect cancellation.
        headers["Host"] = endpoint(profile["public_origin"])[0].netloc
    binding = build_binding(profile)
    oracle = GitOracle(Path(profile["source_repository"]), args.revision)
    workloads = wire_workloads(profile, args.phase)
    require(any(args.text_path in item["paths"] for item in workloads.values()), "text path missing from workload")
    for workload in workloads.values():
        for path in workload["paths"]:
            oracle.prepare(workload, path)
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {"schema":SCHEMA, "status":"running", "binding":binding,
              "profile_sha256":sha256(args.profile), "fixture_commit":oracle.revision,
              "workload_sha256":workload_identity(profile),
              "phase":args.phase, "wire_actions":{name:item["action"] for name, item in workloads.items()},
              "cursor_receipt_sha256":{name:sha256(Path(item["cursor_receipt"])) for name, item in workloads.items() if item.get("cursor_receipt")},
              "driver_sha256":sha256(Path(__file__)), "provider":profile["provider"],
              "timing_definitions":{"ttfb_us":"first response body byte after headers",
                                    "server_timing_ms":"open=capture; read=query; encode=serialization including CPU admission; legacy read includes encode when no encode metric exists; total=handler",
                                    "offered_requests_per_second":"closed-loop attempts divided by measured duration"},
              "region":profile["region"], "resources":profile["resources"], "cases":[]}
    (args.output / "profile.json").write_text(json.dumps(profile, indent=2) + "\n")
    output = args.output / "report.json"
    seen_starts = set()
    try:
        with (args.output / "requests.jsonl").open("x") as journal:
            for case in profile["cases"]:
                state = state_receipt(case, binding)
                require(len(state["replicas"]) == len(urls), "missing replica state receipt")
                starts = {replica["process_start_identity"] for replica in state["replicas"]}
                require(len(starts) == len(urls), "replica receipts identify the same process")
                if case["cache"] in {"cold", "restart"}:
                    require(not starts & seen_starts, "cold/restart case reused an already sampled process")
                seen_starts.update(starts)
                start = journal.tell()
                elapsed = run_case(case, workloads, urls, headers, args.repository, oracle, journal)
                journal.flush()
                report["pending_case"] = {"name":case["name"], "elapsed_seconds":elapsed,
                                          "journal_start":start, "journal_end":journal.tell()}
                # Preserve the measured loop before potentially expensive
                # reporting, so analysis failure cannot erase timing evidence.
                output.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
                summaries = summarize(read_journal(args.output / "requests.jsonl", start, journal.tell()),
                                      elapsed, args.output)
                report["cases"].append({"name":case["name"], "cache":case["cache"], "state":state,
                                        "workloads":summaries})
                report.pop("pending_case")
                output.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
        report["failures"] = validate_report(report, profile)
        if profile.get("baseline_report"):
            baseline_path = Path(profile["baseline_report"])
            report["failures"].extend(compare_baseline(report, json.loads(baseline_path.read_text()), profile))
            report["baseline_sha256"] = sha256(baseline_path)
        report["status"] = "failed" if report["failures"] else "measured"
        # Timing samples alone cannot prove resource/lifecycle or hosted support.
        report["qualification"] = "pending_resource_provider_and_lifecycle_gates"
    except BaseException as error:
        report["status"] = "failed"
        report["error_kind"] = type(error).__name__
        raise
    finally:
        output.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
    print(json.dumps({"status":report["status"], "cases":len(report["cases"])}))
    return 1 if report["status"] == "failed" else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, OSError, subprocess.SubprocessError, KeyError, TypeError) as error:
        failure = {"status":"failed", "error_kind":type(error).__name__}
        if type(error) is ValueError:
            failure["reason"] = str(error)
        print(json.dumps(failure))
        raise SystemExit(1)
