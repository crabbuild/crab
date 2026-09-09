#!/usr/bin/env python3
"""Run SDK qualification cases and aggregate controlled read measurements."""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import platform
import re
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path
import shutil
import tempfile
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[2]
TEST_RESULT = re.compile(
    r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;"
)
HEX_DIGEST = re.compile(r"[0-9a-f]{64}")


def version(command: list[str]) -> str | None:
    try:
        result = subprocess.run(command, check=False, capture_output=True, text=True, timeout=15)
    except OSError:
        return None
    output = (result.stdout or result.stderr).strip().splitlines()
    return output[0] if result.returncode == 0 and output else None


def rust_command(program: str, toolchain: str | None) -> list[str]:
    command = [program]
    if toolchain:
        command.append(f"+{toolchain}")
    command.append("--version")
    return command


def linux_processes() -> dict[int, tuple[int, int]]:
    processes = {}
    for status in Path("/proc").glob("[0-9]*/status"):
        try:
            fields = dict(
                line.split(":", 1) for line in status.read_text(errors="replace").splitlines()
                if ":" in line
            )
            pid = int(status.parent.name)
            parent = int(fields.get("PPid", "0").strip())
            rss = fields.get("VmRSS", "0").strip().split()[0]
            processes[pid] = (parent, int(rss) * 1024)
        except (OSError, ValueError, IndexError):
            continue
    return processes


def unix_processes() -> dict[int, tuple[int, int]]:
    try:
        output = subprocess.run(
            ["ps", "-axo", "pid=,ppid=,rss="], check=False, capture_output=True, text=True
        ).stdout
    except OSError:
        return {}
    processes = {}
    for line in output.splitlines():
        fields = line.split()
        if len(fields) != 3:
            continue
        try:
            pid, parent, rss = (int(value) for value in fields)
        except ValueError:
            continue
        processes[pid] = (parent, rss * 1024)
    return processes


def windows_processes() -> dict[int, tuple[int, int]]:
    script = (
        "Get-CimInstance Win32_Process | ForEach-Object {"
        "$p=Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue;"
        "if($p){'{0} {1} {2}' -f $_.ProcessId,$_.ParentProcessId,$p.WorkingSet64}}"
    )
    try:
        output = subprocess.run(
            ["powershell", "-NoProfile", "-Command", script],
            check=False, capture_output=True, text=True,
        ).stdout
    except OSError:
        return {}
    processes = {}
    for line in output.splitlines():
        try:
            pid, parent, rss = (int(value) for value in line.split())
        except ValueError:
            continue
        processes[pid] = (parent, rss)
    return processes


def process_tree_rss(root: int) -> int:
    if sys.platform.startswith("linux"):
        processes = linux_processes()
    elif os.name == "nt":
        processes = windows_processes()
    else:
        processes = unix_processes()
    children: dict[int, list[int]] = {}
    for pid, (parent, _) in processes.items():
        children.setdefault(parent, []).append(pid)
    pending = [root]
    tree = set()
    while pending:
        pid = pending.pop()
        if pid in tree:
            continue
        tree.add(pid)
        pending.extend(children.get(pid, ()))
    return sum(processes[pid][1] for pid in tree if pid in processes)


def sha256(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def transport_report(path: Path | None) -> tuple[dict | None, bool]:
    if path is None:
        return None, True
    try:
        payload = json.loads(path.read_text())
        requests = payload["requests"]
        reads = sum(requests.get(method, 0) for method in ("GET", "HEAD"))
        values = (
            reads,
            payload["response_body_bytes"],
            payload["write_attempts"],
            payload["proxy_failures"],
        )
        if not all(type(value) is int and value >= 0 for value in values):
            raise ValueError("transport counters must be nonnegative integers")
        report = {
            "read_requests": reads,
            "read_response_bytes": payload["response_body_bytes"],
            "write_requests": payload["write_attempts"],
            "failures": payload["proxy_failures"],
        }
        valid = bool(payload.get("verified")) and reads > 0 and values[2:] == (0, 0)
        return report, valid
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        return {"error": str(error)}, False


def run_case(args: argparse.Namespace) -> None:
    if not args.command:
        raise SystemExit("qualification command is required after --")
    if not HEX_DIGEST.fullmatch(args.fixture_digest):
        raise SystemExit("fixture digest must be a lowercase SHA-256")
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    started = time.monotonic()
    child = subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    peak_rss = 0
    stop = threading.Event()

    def sample() -> None:
        nonlocal peak_rss
        while not stop.wait(0.05):
            peak_rss = max(peak_rss, process_tree_rss(child.pid))

    sampler = threading.Thread(target=sample, name="sdk-rss-sampler", daemon=True)
    sampler.start()
    terminal_state = "exited"
    try:
        stdout, stderr = child.communicate(timeout=args.timeout_seconds)
    except subprocess.TimeoutExpired:
        terminal_state = "timed_out"
        child.kill()
        stdout, stderr = child.communicate()
    finally:
        peak_rss = max(peak_rss, process_tree_rss(child.pid))
        stop.set()
        sampler.join()
    elapsed = time.monotonic() - started
    totals = [tuple(map(int, match)) for match in TEST_RESULT.findall(stdout + stderr)]
    passed = sum(item[0] for item in totals)
    failed = sum(item[1] for item in totals)
    ignored = sum(item[2] for item in totals)
    tests_valid = args.expected_tests is None or (
        passed == args.expected_tests and failed == 0 and ignored == 0
    )
    rss_valid = args.require_rss_below is None or (
        0 < peak_rss <= args.require_rss_below
    )
    transport, transport_valid = transport_report(args.transport_report)
    succeeded = (child.returncode == 0 and terminal_state == "exited" and tests_valid
                 and rss_valid and transport_valid)
    report = {
        "schema": "crab.sdk-qualification",
        "version": 1,
        "source_sha": version(["git", "rev-parse", "HEAD"]),
        "feature_set": sorted(set(filter(None, args.features.split(",")))),
        "tool_versions": {
            "python": platform.python_version(),
            "platform": platform.platform(),
            "git": version([args.git, "--version"]),
            "rustc": version(rust_command("rustc", args.toolchain)),
            "cargo": version(rust_command("cargo", args.toolchain)),
            "crab": version([args.crab, "--version"]) if args.crab else None,
        },
        "backend": args.backend,
        "fixture_digest": args.fixture_digest,
        "command": command,
        "timing": {"wall_seconds": round(elapsed, 6)},
        "peak_rss_bytes": peak_rss,
        "tests": {"passed": passed, "failed": failed, "ignored": ignored},
        "limits": {"peak_rss_bytes": args.require_rss_below},
        "transport": transport,
        "terminal_state": "passed" if succeeded else terminal_state if terminal_state != "exited" else "failed",
        "exit_code": child.returncode,
        "stdout_sha256": hashlib.sha256(stdout.encode()).hexdigest(),
        "stderr_sha256": hashlib.sha256(stderr.encode()).hexdigest(),
    }
    output.write_text(json.dumps(report, indent=2) + "\n")
    output.with_suffix(".stdout.log").write_text(stdout)
    output.with_suffix(".stderr.log").write_text(stderr)
    if not succeeded:
        raise SystemExit(1)


def measurement(path: Path) -> dict:
    report = json.loads(path.read_text())
    required = ("elapsed_seconds", "peak_rss_bytes", "verified", "terminal_state")
    if any(field not in report for field in required):
        raise ValueError(f"incomplete measurement: {path}")
    if not report["verified"] or report["terminal_state"] != "exited":
        raise ValueError(f"failed measurement: {path}")
    return report


def compare_read(args: argparse.Namespace) -> None:
    manifest = json.loads(args.manifest.read_text())
    trials = manifest.get("trials", [])
    groups: dict[tuple[str, str], list[dict]] = {}
    for trial in trials:
        key = (trial.get("implementation"), trial.get("cache_state"))
        if key not in {("core", "cold"), ("core", "warm"), ("sdk", "cold"), ("sdk", "warm")}:
            raise ValueError(f"invalid read trial group: {key}")
        measured = measurement((args.manifest.parent / trial["measurement"]).resolve())
        origin = json.loads((args.manifest.parent / trial["origin_metrics"]).read_text())
        requests = origin.get("read_requests")
        response_bytes = origin.get("read_response_bytes")
        writes = origin.get("write_requests")
        failures = origin.get("failures")
        if not all(type(value) is int and value >= 0
                   for value in (requests, response_bytes, writes, failures)):
            raise ValueError("origin metrics must contain nonnegative integer counters")
        if writes != 0 or failures != 0:
            raise ValueError("read qualification observed an origin write or transport failure")
        groups.setdefault(key, []).append({
            "wall_seconds": measured["elapsed_seconds"],
            "peak_rss_bytes": measured["peak_rss_bytes"],
            "origin_requests": requests,
            "origin_response_bytes": response_bytes,
        })
    if any(len(groups.get(key, [])) != 5 for key in (
        ("core", "cold"), ("core", "warm"), ("sdk", "cold"), ("sdk", "warm")
    )):
        raise ValueError("read qualification requires five trials in each core/SDK cold/warm group")
    medians = {
        f"{implementation}_{cache}": {
            field: statistics.median(trial[field] for trial in values)
            for field in ("wall_seconds", "origin_requests", "origin_response_bytes")
        }
        for (implementation, cache), values in groups.items()
    }
    if any(medians[f"core_{cache}"][field] <= 0 for cache in ("cold", "warm")
           for field in ("wall_seconds", "origin_requests", "origin_response_bytes")):
        raise ValueError("shared-core baselines must contain positive timing and origin traffic")
    ratios = {
        cache: {
            field: medians[f"sdk_{cache}"][field] / medians[f"core_{cache}"][field]
            if medians[f"core_{cache}"][field] else 1.0
            for field in ("wall_seconds", "origin_requests", "origin_response_bytes")
        }
        for cache in ("cold", "warm")
    }
    max_rss = max(trial["peak_rss_bytes"] for values in groups.values() for trial in values)
    passed = max_rss <= 512 * 1024 * 1024 and all(
        ratio <= 1.10 for values in ratios.values() for ratio in values.values()
    )
    report = {
        "schema": "crab.sdk-read-performance",
        "version": 1,
        "source_sha": manifest.get("source_sha"),
        "feature_set": ["remote", "content"],
        "tool_versions": manifest.get("tool_versions"),
        "backend": manifest.get("backend"),
        "fixture_digest": manifest.get("fixture_digest"),
        "runner": manifest.get("runner"),
        "trials": {
            f"{implementation}_{cache}": values
            for (implementation, cache), values in groups.items()
        },
        "medians": medians,
        "sdk_to_core_ratios": ratios,
        "peak_rss_bytes": max_rss,
        "terminal_state": "passed" if passed else "failed",
    }
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    if not passed:
        raise SystemExit(1)


class ReadOrigin(ThreadingHTTPServer):
    daemon_threads = False

    def __init__(self, upstream):
        super().__init__(("127.0.0.1", 0), ReadOriginHandler)
        self.upstream = upstream
        self.counts = Counter()
        self.response_bytes = 0
        self.failures = 0
        self.lock = threading.Lock()

    def snapshot(self) -> tuple[Counter, int, int]:
        with self.lock:
            return self.counts.copy(), self.response_bytes, self.failures


class ReadOriginHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args) -> None:
        pass

    def send_error(self, code, message=None, explain=None) -> None:
        if code == 501:
            with self.server.lock:
                self.server.counts[self.command] += 1
        super().send_error(code, message, explain)

    def reject_write(self) -> None:
        with self.server.lock:
            self.server.counts[self.command] += 1
        self.send_error(403, "qualification forbids writes")
        self.close_connection = True

    do_PUT = do_POST = do_DELETE = do_PATCH = reject_write

    def forward_read(self) -> None:
        with self.server.lock:
            self.server.counts[self.command] += 1
        connection = http.client.HTTPConnection(
            self.server.upstream.hostname, self.server.upstream.port, timeout=180
        )
        headers = dict(self.headers.items())
        headers["Connection"] = "close"
        transferred = 0
        try:
            connection.request(self.command, self.path, headers=headers)
            response = connection.getresponse()
            self.send_response_only(response.status)
            for name, value in response.getheaders():
                if name.lower() not in {"connection", "transfer-encoding"}:
                    self.send_header(name, value)
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD":
                while data := response.read(64 * 1024):
                    self.wfile.write(data)
                    transferred += len(data)
        except (OSError, http.client.HTTPException):
            with self.server.lock:
                self.server.failures += 1
        finally:
            connection.close()
            with self.server.lock:
                self.server.response_bytes += transferred
            self.close_connection = True

    do_GET = do_HEAD = forward_read


def benchmark_read(args: argparse.Namespace) -> None:
    if not sys.platform.startswith("linux"):
        raise SystemExit("controlled read qualification requires Linux")
    if not HEX_DIGEST.fullmatch(args.fixture_digest):
        raise SystemExit("fixture digest must be a lowercase SHA-256")
    if args.bytes <= 0 or args.cache_bytes <= 0:
        raise SystemExit("fixture and cache sizes must be positive")
    args.core = args.core.resolve(strict=True)
    args.sdk = args.sdk.resolve(strict=True)
    args.cache_root = args.cache_root.resolve(strict=True)
    if not args.cache_root.is_dir():
        raise SystemExit("cache root must be an existing directory")
    upstream = urlsplit(args.upstream)
    if (upstream.scheme != "http" or not upstream.hostname or upstream.username
            or upstream.password or upstream.path not in {"", "/"}
            or upstream.query or upstream.fragment):
        raise SystemExit("upstream must be an HTTP endpoint without credentials or path")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    source_sha = version(["git", "rev-parse", "HEAD"])
    manifest = {
        "source_sha": source_sha,
        "tool_versions": {
            "python": platform.python_version(),
            "platform": platform.platform(),
            "git": version([args.git, "--version"]),
            "rustc": version(rust_command("rustc", args.toolchain)),
            "cargo": version(rust_command("cargo", args.toolchain)),
        },
        "backend": args.backend,
        "fixture_digest": args.fixture_digest,
        "runner": platform.node(),
        "trials": [],
    }
    proxy = ReadOrigin(upstream)
    worker = threading.Thread(target=proxy.serve_forever, name="sdk-read-origin")
    worker.start()
    environment = os.environ.copy()
    environment["AWS_ENDPOINT_URL_S3"] = f"http://127.0.0.1:{proxy.server_port}"
    try:
        for trial in range(5):
            implementations = (("core", args.core), ("sdk", args.sdk))
            if trial % 2:
                implementations = tuple(reversed(implementations))
            for implementation, executable in implementations:
                cache = Path(tempfile.mkdtemp(prefix=f"sdk-{implementation}-{trial}-",
                                              dir=args.cache_root))
                try:
                    for cache_state in ("cold", "warm"):
                        stem = f"{implementation}-{cache_state}-{trial + 1}"
                        before_counts, before_bytes, before_failures = proxy.snapshot()
                        command = [
                            sys.executable, str(ROOT / "crab/scripts/measure-sdk-read.py"),
                            str(executable), args.bucket, args.repository, args.branch,
                            args.path, str(cache), "--commit", args.commit,
                            "--bytes", str(args.bytes), "--blake3", args.blake3,
                            "--cache-bytes", str(args.cache_bytes),
                        ]
                        result = subprocess.run(command, cwd=ROOT, env=environment,
                                                check=False, capture_output=True, text=True)
                        measurement_path = output / f"{stem}.json"
                        measurement_path.write_text(result.stdout)
                        (output / f"{stem}.stderr.log").write_text(result.stderr)
                        after_counts, after_bytes, after_failures = proxy.snapshot()
                        delta = after_counts - before_counts
                        origin_path = output / f"{stem}-origin.json"
                        origin_path.write_text(json.dumps({
                            "read_requests": delta["GET"] + delta["HEAD"],
                            "read_response_bytes": after_bytes - before_bytes,
                            "write_requests": sum(count for method, count in delta.items()
                                                  if method not in {"GET", "HEAD"}),
                            "failures": after_failures - before_failures,
                        }, indent=2) + "\n")
                        manifest["trials"].append({
                            "implementation": implementation,
                            "cache_state": cache_state,
                            "measurement": measurement_path.name,
                            "origin_metrics": origin_path.name,
                        })
                        if result.returncode != 0:
                            raise SystemExit(1)
                finally:
                    shutil.rmtree(cache, ignore_errors=True)
    finally:
        proxy.shutdown()
        worker.join()
        proxy.server_close()
    manifest_path = output / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    compare_read(argparse.Namespace(manifest=manifest_path, output=output / "report.json"))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="subcommand", required=True)
    run = commands.add_parser("run", help="run one isolated qualification command")
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("--backend", required=True)
    run.add_argument("--features", default="")
    run.add_argument("--fixture-digest", required=True)
    run.add_argument("--expected-tests", type=int)
    run.add_argument("--require-rss-below", type=int)
    run.add_argument("--timeout-seconds", type=int, default=1800)
    run.add_argument("--git", default="git")
    run.add_argument("--crab")
    run.add_argument("--toolchain")
    run.add_argument("--transport-report", type=Path)
    run.add_argument("command", nargs=argparse.REMAINDER)
    run.set_defaults(action=run_case)
    compare = commands.add_parser("compare-read", help="verify five paired cold/warm reads")
    compare.add_argument("--manifest", type=Path, required=True)
    compare.add_argument("--output", type=Path, required=True)
    compare.set_defaults(action=compare_read)
    benchmark = commands.add_parser(
        "benchmark-read", help="run five shared-core/SDK cold/warm read pairs"
    )
    benchmark.add_argument("--output", type=Path, required=True)
    benchmark.add_argument("--cache-root", type=Path, required=True)
    benchmark.add_argument("--core", type=Path, required=True)
    benchmark.add_argument("--sdk", type=Path, required=True)
    benchmark.add_argument("--upstream", required=True)
    benchmark.add_argument("--backend", required=True)
    benchmark.add_argument("--fixture-digest", required=True)
    benchmark.add_argument("--bucket", required=True)
    benchmark.add_argument("--repository", required=True)
    benchmark.add_argument("--branch", required=True)
    benchmark.add_argument("--path", required=True)
    benchmark.add_argument("--commit", required=True)
    benchmark.add_argument("--bytes", type=int, required=True)
    benchmark.add_argument("--blake3", required=True)
    benchmark.add_argument("--cache-bytes", type=int, default=2 * 1024 * 1024 * 1024)
    benchmark.add_argument("--git", default="git")
    benchmark.add_argument("--toolchain")
    benchmark.set_defaults(action=benchmark_read)
    args = parser.parse_args()
    args.action(args)


if __name__ == "__main__":
    main()
