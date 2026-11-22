#!/usr/bin/env python3
"""Verify SDK hydration or a native SDK stream against a protected xorb fixture."""

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
    COMMIT, RUN_NUMBER, SDK_TIMING_FIELDS, SDK_XORB_RANGE_READ, decode, fields, matches,
    read_git_record, require, scope_parts, validate_xorb_publication, validate_xorb_workload, verify_sdk_candidate,
    verify_sparse_xorb_traffic, verify_xorb_read,
)
from run_cdn_git_workload import runner_platform, viewer_binding_sha256
from run_cdn_sdk_workload import binding_for, json_record, observed_interval, split_observations, verified_target
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
    reader = {name: value for name, value in expected.items() if name != "message"}
    return xorb_evidence(child_lines, fixture, key, reader, arm)


def xorb_evidence(lines: list[bytes], fixture: dict, key: str, reader: dict, arm: str, *, sparse: bool = False) -> dict:
    responses = {}
    key_sha256 = digest(key.encode())
    source = "local" if arm == "warm_local" else "edge" if sparse and arm == "edge" else None
    counts = measure(lines, fixture["scope"], expected_xorb_key=key_sha256, edge_responses=responses,
                     required_xorb_source=source)
    native = measure_native(lines, fixture["scope"])
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
    evidence = counts | {"reader": reader, "xorb_key_sha256": key_sha256, "native_traffic": native, "edge_responses": responses}
    if sparse and arm == "edge":
        verify_sparse_xorb_traffic(evidence, fixture["size"])
    return evidence


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
    spec = validate_xorb_workload(read_git_record(args.workload), args.workload_sha256,
                                  args.configuration_sha256, allow_sparse=True)
    window = spec.get("read_range")
    require(spec["global_prefix_sha256"] == digest(key.rsplit("/xorbs/", 1)[0].encode()),
            "hydration workload has a different projected global prefix")
    pointer = read_git_record(args.pointer, limit=4096)
    require(digest(pointer) == spec["pointer_sha256"], "fixture pointer differs from the approved workload")
    probe = args.probe.resolve(strict=True)
    probe_digest = file_digest(probe)
    platform = runner_platform()
    row = {"scope": fixture["scope"], "configuration_sha256": args.configuration_sha256,
           "source_sha": args.source_sha, "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
           "platform": platform}
    args.export_approval = None
    binding, approval, _ = binding_for(args, fixture)
    expected_payload = {"sha256": spec["payload_sha256"], "bytes": spec["payload_bytes"]}
    request = {
        "origin": fixture["native_origin"], "repository_prefix": fixture["fixture_prefix"], "base_url": fixture["base_url"],
        "branch": "", "expected_oid": "", "expected_sha256": expected_payload["sha256"], "expected_bytes": expected_payload["bytes"],
        "binding": binding, "repeat_snapshot": False, "public_transition": False,
    }
    if window is None:
        require(args.git is not None and args.crab is not None and args.receipt is None,
                "SDK worktree hydration requires Git and Crab executables")
        git, crab = (path.resolve(strict=True) for path in (args.git, args.crab))
        require(file_digest(crab) == spec["crab_sha256"], "hydration child differs from the approved Crab build")
        request["worktree"] = {"git": str(git), "crab": str(crab), "hydration": {
            "remote": native_remote(fixture, edge), "pointer": pointer.decode("utf-8"), "repeat": repeat,
        }}
    else:
        require(args.receipt is not None and args.git is None and args.crab is None and not repeat,
                "SDK sparse reads require a native publication receipt and a fresh remote snapshot")
        receipt = read_git_record(args.receipt)
        publication = validate_xorb_publication(receipt, row)
        require(digest(receipt) == spec["publication_sha256"] and publication["published"]
                and publication["sha256"] == spec["pointer_sha256"]
                and publication["candidate"]["binary_sha256"] == spec["crab_sha256"],
                "SDK sparse read requires the independently published pointer")
        require(probe_digest == spec["sdk_sha256"], "SDK sparse read requires the approved SDK executable")
        expected_payload = {name: window[name] for name in ("sha256", "bytes")}
        request |= {"branch": publication["ref"].removeprefix("refs/heads/"), "expected_oid": spec["projected_oid"],
                    "expected_sha256": expected_payload["sha256"], "expected_bytes": expected_payload["bytes"]}
    env = {name: value for name, value in os.environ.items()
           if not name.upper().startswith("GIT_") and name not in ("CRAB_LOG", "CRAB_OTLP_ENDPOINT")}
    with tempfile.TemporaryDirectory(prefix="crab-sdk-hydration-") as directory:
        cache = Path(directory).resolve() / "cache"
        env |= {"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_TERMINAL_PROMPT": "0",
                "CRAB_CACHE_DIR": str(cache)}
        start = time.time_ns() // 1_000_000
        if window is not None:
            require(publication["visible_at_unix_ms"] <= start, "SDK sparse read started before native publication")
            request["xorb_range"] = {"offset": window["offset"], "file_bytes": spec["payload_bytes"],
                                     "pointer_sha256": spec["pointer_sha256"], "cache_root": str(cache)}
        try:
            result = subprocess.run([str(probe)], input=json.dumps(request).encode(), env=env,
                                    capture_output=True, timeout=300, check=False)
        except subprocess.TimeoutExpired:
            raise ValueError("SDK hydration workload timed out") from None
        end = time.time_ns() // 1_000_000
        require(result.returncode == 0, "SDK hydration failed to verify the file bytes")
        report_fields = SDK_TIMING_FIELDS | {"payload", "build", "warm_local"}
        if window is not None:
            report_fields |= {"commit", "range_offset", "cache_root_sha256"}
        report = fields(decode(result.stdout), report_fields, "SDK hydration report")
        payload = fields(report["payload"], {"sha256", "bytes"}, "hydrated file")
        require(type(payload["bytes"]) is int and payload == expected_payload,
                "SDK hydration did not verify the approved file")
        build = report["build"]
        require(isinstance(build, dict) and "binary_sha256" not in build, "invalid SDK hydration build")
        candidate = verify_sdk_candidate(build | {"binary_sha256": probe_digest}, args.source_sha, platform)
        require("local" in candidate["cargo_features"].split(","), "SDK hydration requires the local feature")
        timing = observed_interval({name: report[name] for name in SDK_TIMING_FIELDS}, start, end)
        lines, local_lines = split_observations(result.stderr.splitlines(), repeat)
        context = row | {
            "schema": "crab.cdn-sdk-hydration/v1", "workload_sha256": args.workload_sha256,
            "candidate": candidate, "crab_sha256": spec["crab_sha256"],
            "pointer_sha256": spec["pointer_sha256"], "payload": payload,
        }
        if window is not None:
            require(report["warm_local"] is None and report["cache_root_sha256"] == digest(os.fsencode(cache)),
                    "SDK stream did not confirm its isolated cache")
            projected = fixture | {"fixture_prefix": fixture["fixture_prefix"] + fixture["view_suffix"]}
            reader = verified_target(lines, projected, args.arm, approval)
            reader.pop("public_export_sha256")
            reader |= {"approval_sha256": approval, "global_prefix_sha256": spec["global_prefix_sha256"],
                       "local_cache_only": True, "cache_root_sha256": report["cache_root_sha256"]}
            context |= {"schema": SDK_XORB_RANGE_READ, "range_offset": report["range_offset"], "commit": report["commit"]}
            primary = context | timing | xorb_evidence(lines, fixture, key, reader, args.arm, sparse=True) | {"arm": args.arm}
            registration = {"hydration_workload_sha256": args.workload_sha256, "sdk_candidate": candidate,
                            "viewer_binding_sha256": reader["binding_sha256"], "viewer_approval_sha256": approval}
            placement = {"origin_sha256": reader["origin_sha256"], "views": {"target": {
                "physical_prefix_sha256": reader["prefix_sha256"], "key_sha256": digest(key.encode()),
            }}}
            return verify_xorb_read(json_record(primary).encode(), row, registration, args.arm, spec, placement), None
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
    for name in ("fixture", "workload", "pointer", "probe", "private-reader"):
        parser.add_argument("--" + name, required=True, type=Path)
    for name in ("git", "crab", "receipt"):
        parser.add_argument("--" + name, type=Path)
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
