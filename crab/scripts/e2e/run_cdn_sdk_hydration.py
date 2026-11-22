#!/usr/bin/env python3
"""Verify an SDK hydration child against a protected xorb fixture."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from collect_cdn_product_metrics import PRODUCT_COUNTERS, measure, measure_native
from cdn_qualification import (
    COMMIT, RUN_NUMBER, SDK_TIMING_FIELDS, decode, fields, matches,
    read_git_record, require, scope_parts, validate_xorb_workload, verify_sdk_candidate,
)
from run_cdn_git_workload import runner_platform, viewer_binding_sha256
from run_cdn_sdk_workload import binding_for, json_record, observed_interval, split_observations
from run_cdn_transport_checks import approved_fixture, fixture_object_key, native_remote


CHILD = "crab_sdk::cdn::child"
TARGET = "crab::cdn::checkout_target"


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def file_digest(path: Path) -> str:
    with path.open("rb") as data:
        return hashlib.file_digest(data, "sha256").hexdigest()


def child_evidence(lines: list[bytes], fixture: dict, key: str, cache: Path, approval: str, arm: str) -> dict:
    expected = {
        "message": "CDN child captured read target",
        "origin_sha256": digest(fixture["native_origin"].encode()),
        "prefix_sha256": digest(f"{fixture['fixture_prefix']}{fixture['view_suffix']}/".encode()),
        "global_prefix_sha256": digest(key.rsplit("/xorbs/", 1)[0].encode()),
        "binding_sha256": viewer_binding_sha256(fixture) if approval else "",
        "approval_sha256": approval,
        "cache_root_sha256": digest(os.fsencode(cache)),
        "local_cache_only": True,
    }
    child_lines = []
    targets = []
    for line in lines:
        if CHILD.encode() not in line:
            continue
        event = decode(line)
        if not isinstance(event, dict) or event.get("target") != CHILD:
            continue
        relay = fields(event.get("fields"), {"observation"}, "SDK child observation")
        require(isinstance(relay["observation"], str), "invalid SDK child observation")
        observation = fields(decode(relay["observation"].encode()), {"target", "fields"}, "redacted Crab child event")
        if observation["target"] == TARGET:
            targets.append(observation["fields"])
        else:
            child_lines.append(relay["observation"].encode())
    require(targets and all(target == expected for target in targets),
            "hydration child did not confirm the approved projected view, binding, and isolated cache")
    responses = {}
    key_sha256 = digest(key.encode())
    counts = measure(child_lines, fixture["scope"], expected_xorb_key=key_sha256, edge_responses=responses,
                     local_xorb_only=arm == "warm_local")
    native = measure_native(child_lines, fixture["scope"])
    if arm == "warm_local":
        require(counts["local_payload_hits"] > 0
                and all(counts[name] == 0 for name in PRODUCT_COUNTERS - {"local_payload_hits"}),
                "SDK warm hydration requires verified local chunks with zero edge attempts")
    elif arm == "edge":
        require(counts["observed_edge_gets"] > 0 and counts["verified_edge_payloads"] > 0
                and counts["local_payload_hits"] == 0,
                "hydration child did not accept verified chunks from the approved CDN xorb")
    else:
        require(all(counts[name] == 0 for name in PRODUCT_COUNTERS)
                and native["get_responses"] > 0 and native["body_bytes"] > 0,
                "native hydration control requires origin reads and zero edge attempts")
    reader = {name: value for name, value in expected.items() if name != "message"}
    return counts | {"reader": reader, "xorb_key_sha256": key_sha256, "native_traffic": native, "edge_responses": responses}


def hydrate(args: argparse.Namespace) -> tuple[dict, dict | None]:
    require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
            and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
    repeat = args.warm_local_output is not None
    require(not repeat or args.arm == "edge", "a warm local hydration requires its own edge primer")
    fixture = approved_fixture(args.fixture, args.configuration_sha256)
    edge, access, consumer, family, profile = scope_parts(fixture["scope"])
    require((access, consumer, family, profile) == ("private", "sdk", "xorb", "direct"),
            "SDK hydration requires a private direct SDK xorb fixture")
    key = fixture_object_key(fixture, access, family)
    spec = validate_xorb_workload(read_git_record(args.workload), args.workload_sha256, args.configuration_sha256)
    require(spec["global_prefix_sha256"] == digest(key.rsplit("/xorbs/", 1)[0].encode()),
            "hydration workload has a different projected global prefix")
    pointer = read_git_record(args.pointer, limit=4096)
    require(digest(pointer) == spec["pointer_sha256"], "fixture pointer differs from the approved workload")
    git, crab, probe = (path.resolve(strict=True) for path in (args.git, args.crab, args.probe))
    require(file_digest(crab) == spec["crab_sha256"], "hydration child differs from the approved Crab build")
    probe_digest = file_digest(probe)
    args.export_approval = None
    binding, approval, _ = binding_for(args, fixture)
    request = {
        "origin": fixture["native_origin"], "repository_prefix": fixture["fixture_prefix"], "base_url": fixture["base_url"],
        "branch": "", "expected_oid": "", "expected_sha256": spec["payload_sha256"], "expected_bytes": spec["payload_bytes"],
        "binding": binding, "repeat_snapshot": False, "public_transition": False,
        "worktree": {"git": str(git), "crab": str(crab), "hydration": {
            "remote": native_remote(fixture, edge), "pointer": pointer.decode("utf-8"), "repeat": repeat,
        }},
    }
    env = {name: value for name, value in os.environ.items()
           if not name.upper().startswith("GIT_") and name not in ("CRAB_LOG", "CRAB_OTLP_ENDPOINT")}
    with tempfile.TemporaryDirectory(prefix="crab-sdk-hydration-") as directory:
        cache = Path(directory).resolve() / "cache"
        env |= {"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_TERMINAL_PROMPT": "0",
                "CRAB_CACHE_DIR": str(cache)}
        start = time.time_ns() // 1_000_000
        try:
            result = subprocess.run([str(probe)], input=json.dumps(request).encode(), env=env,
                                    capture_output=True, timeout=300, check=False)
        except subprocess.TimeoutExpired:
            raise ValueError("SDK hydration workload timed out") from None
        end = time.time_ns() // 1_000_000
        require(result.returncode == 0, "SDK hydration failed to verify the hydrated file")
        report = fields(decode(result.stdout), SDK_TIMING_FIELDS | {"payload", "build", "warm_local"}, "SDK hydration report")
        payload = fields(report["payload"], {"sha256", "bytes"}, "hydrated file")
        require(payload["sha256"] == spec["payload_sha256"] and type(payload["bytes"]) is int
                and payload["bytes"] == spec["payload_bytes"],
                "SDK hydration did not verify the approved file")
        platform = runner_platform()
        build = report["build"]
        require(isinstance(build, dict) and "binary_sha256" not in build, "invalid SDK hydration build")
        candidate = verify_sdk_candidate(build | {"binary_sha256": probe_digest}, args.source_sha, platform)
        require("local" in candidate["cargo_features"].split(","), "SDK hydration requires the local feature")
        timing = observed_interval({name: report[name] for name in SDK_TIMING_FIELDS}, start, end)
        lines, local_lines = split_observations(result.stderr.splitlines(), repeat)
        context = {
            "schema": "crab.cdn-sdk-hydration/v1", "scope": fixture["scope"],
            "configuration_sha256": args.configuration_sha256, "workload_sha256": args.workload_sha256,
            "source_sha": args.source_sha, "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
            "platform": platform, "candidate": candidate, "crab_sha256": spec["crab_sha256"],
            "pointer_sha256": spec["pointer_sha256"], "payload": payload,
        }
        primary = context | timing | child_evidence(lines, fixture, key, cache, approval, args.arm) | {"arm": args.arm}
        local = None
        if repeat:
            local_timing = observed_interval(report["warm_local"], primary["finished_at_unix_ms"], end)
            local = context | local_timing | child_evidence(local_lines, fixture, key, cache, approval, "warm_local") | {
                "arm": "warm_local", "primer_sha256": digest(json_record(primary).encode()),
            }
        else:
            require(report["warm_local"] is None, "unexpected SDK hydration repeat output")
    return primary, local


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("arm", choices=("edge", "origin"))
    for name in ("fixture", "workload", "pointer", "git", "crab", "probe", "private-reader"):
        parser.add_argument("--" + name, required=True, type=Path)
    for name in ("configuration-sha256", "workload-sha256", "source-sha", "run-id", "run-attempt"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--warm-local-output", type=Path)
    args = parser.parse_args()
    try:
        primary, local = hydrate(args)
        if local is not None:
            with args.warm_local_output.open("x", encoding="utf-8", newline="\n") as stream:
                stream.write(json_record(local))
        sys.stdout.write(json_record(primary))
    except (OSError, ValueError):
        # A parser failure may involve fixture fields or child diagnostics.
        print("error: SDK hydration evidence could not be verified", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
