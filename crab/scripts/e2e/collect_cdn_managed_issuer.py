#!/usr/bin/env python3
"""Reduce managed CDN product reads into a release issuer observation."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cdn_qualification import (  # noqa: E402
    CDN_PROVENANCE_FIELDS, COMMIT, LABEL, RUN_NUMBER, SHA256, benchmark_registration,
    decode, fields, matches, positive, read_git_record, read_trace, require, scope_parts,
    verify_deployment_policy_trace, verify_managed_issuer,
)
from collect_cdn_product_metrics import NATIVE_TARGET_MARKER, measure, measure_native  # noqa: E402
from run_cdn_transport_checks import approved_fixture  # noqa: E402


TARGET = "crab_auth::managed::cdn"
TARGET_MARKER = TARGET.encode()
RESOLUTION = "Managed CDN repository resolution"
GRANT = "Managed CDN read grant exchange"
ROLES = ("approved_v2", "v1_peer", "denied_v2", "mismatched_v2")
IDENTITY_FIELDS = {"issuer_identity_sha256", "issuer_version", "repository_sha256"}
BINDING_FIELDS = {"binding_approval_sha256", "binding_origin_sha256", "binding_prefix_sha256", "binding_viewer_sha256"}


def observation_events(lines: list[bytes]) -> tuple[dict, dict]:
    events = []
    for line in lines:
        if TARGET_MARKER not in line:
            continue
        item = decode(line)
        if isinstance(item, dict) and item.get("target") == TARGET:
            events.append(item.get("fields"))
    require(len(events) == 2, "each managed arm needs one authenticated resolution and one observed grant POST")
    resolved = fields(events[0], IDENTITY_FIELDS | BINDING_FIELDS | {"message"}, "managed resolution event")
    grant = fields(events[1], IDENTITY_FIELDS | {
        "message", "operation", "request_version", "request_approval_sha256", "status", "grant_version",
        "grant_approval_sha256", "grant_origin_sha256", "grant_prefix_sha256", "accepted",
    }, "managed grant event")
    require((resolved["message"], grant["message"]) == (RESOLUTION, GRANT)
            and all(resolved[key] == grant[key] for key in IDENTITY_FIELDS)
            and matches(SHA256, resolved["repository_sha256"]), "managed grant does not follow the same authenticated repository resolution")
    return resolved, grant


def collect_managed_issuer(
    fixture: dict, report: dict, registration: dict, policy: dict, actions: Path,
) -> dict:
    edge, access, _, family, profile = scope_parts(report["scope"])
    require(access == "private" and profile == "managed" and fixture["scope"] == report["scope"] and fixture["view_suffix"] == "",
            "managed issuer observations require a repository-local private managed fixture")
    require(all(report[key] == registration[key] for key in ("scope", "source_sha", "configuration_sha256"))
            and fixture["deployment_policy_sha256"] == registration["deployment_policy_sha256"],
            "managed issuer observations must match the registered row")
    verify_deployment_policy_trace(policy, report, edge, access, registration["deployment_policy_sha256"])
    issuer = registration["managed_issuer"]
    approval = issuer["approval_sha256"]
    placements = {
        "origin": hashlib.sha256(fixture["native_origin"].encode()).hexdigest(),
        "prefix": hashlib.sha256(fixture["fixture_prefix"].encode()).hexdigest(),
        "viewer": hashlib.sha256(fixture["base_url"].rstrip("/").encode()).hexdigest(),
    }
    expected_binding = {f"binding_{name}_sha256": digest for name, digest in placements.items()} | {
        "binding_approval_sha256": approval,
    }
    action = report["product_action"]
    require(action["name"] == registration["workload"]
            and matches(SHA256, action["observed_sha256"]) and positive(action["observed_bytes"]),
            "managed issuer evidence needs the registered action's verified product result")
    operations = {}
    repositories, observed_operations = set(), set()
    for role in ROLES:
        directory = actions / role
        lines = read_git_record(directory / "stderr.jsonl", limit=8 * 1024 * 1024).splitlines()
        resolved, grant = observation_events(lines)
        require((resolved["issuer_identity_sha256"], resolved["issuer_version"]) == (issuer["identity_sha256"], issuer["version"]),
                "managed action used a different issuer or service version")
        observed = {name: resolved[name] for name in BINDING_FIELDS}
        require(observed == expected_binding or (role == "v1_peer" and observed == dict.fromkeys(BINDING_FIELDS, "")),
                "authenticated managed resolution differs from the approved viewer and native placement")
        repositories.add(resolved["repository_sha256"])
        require(grant["operation"] in (("Hydrate",) if family in ("lfs", "xorb", "chunk-page", "shard") else ("Clone", "Fetch"))
                and all(type(grant[key]) is int for key in ("request_version", "status", "grant_version"))
                and all(value == "" or matches(SHA256, value) for value in
                        (grant["request_approval_sha256"], grant["grant_approval_sha256"])),
                "invalid managed grant observation for the registered action")
        observed_operations.add(grant["operation"])
        successful = role in ("approved_v2", "v1_peer")
        require(grant["accepted"] is successful, "managed action did not accept or reject the grant as expected")
        exit_code = read_git_record(directory / "exit-code", limit=16).strip()
        require(exit_code.isdigit() and (int(exit_code) == 0) == successful,
                "managed product action did not exit with the expected result")
        traffic, native = measure(lines, report["scope"]), measure_native(lines, report["scope"])
        payload = directory / "payload"
        if successful:
            require(all(grant[f"grant_{name}_sha256"] == placements[name] for name in ("origin", "prefix")),
                    "accepted managed grant changed the native target")
            size = payload.stat().st_size
            require(size == action["observed_bytes"], "managed read returned a different payload length")
            with payload.open("rb") as source:
                digest = hashlib.file_digest(source, "sha256").hexdigest()
            require(digest == action["observed_sha256"], "managed read returned different payload bytes")
            if role == "approved_v2":
                require(positive(traffic["verified_edge_payloads"]), "approved managed v2 read verified no edge payload")
            else:
                require(traffic["all_edge_attempts"] == 0 and positive(native["body_bytes"]),
                        "managed v1 peer must read native payload without an edge attempt")
            result = "verified"
        else:
            require(not payload.exists() and traffic["all_edge_attempts"] == 0
                    and not any(NATIVE_TARGET_MARKER in line for line in lines),
                    "rejected managed grant must read no edge or native payload")
            digest, size = None, 0
            result = "issuer_denied" if role == "denied_v2" else "client_rejected"
        operations[role] = {
            "request_versions": [grant["request_version"]], "request_approval_sha256": grant["request_approval_sha256"] or None,
            "status": grant["status"], "grant_version": grant["grant_version"] or None,
            "grant_approval_sha256": grant["grant_approval_sha256"] or None,
            "result": result, "verified_output_sha256": digest, "verified_output_bytes": size,
            "edge_payload_requests": traffic["observed_edge_gets"], "native_payload_reads": native["get_responses"],
        }
    require(len(repositories) == len(observed_operations) == 1, "managed arms must exercise one repository and operation")
    trace = {key: report[key] for key in CDN_PROVENANCE_FIELDS | {"platform"}} | {
        "schema": "crab.cdn-managed-issuer/v1", "issuer": {key: issuer[key] for key in ("identity_sha256", "version")},
        "binding_sha256": approval, "placement_sha256": policy["components"]["private-binding"], "workload": action["name"],
        "operations": operations,
    }
    verify_managed_issuer(trace, report, registration, policy)
    return trace


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--benchmarks", type=Path, required=True)
    parser.add_argument("--benchmarks-sha256", required=True)
    parser.add_argument("--observations", type=Path, required=True)
    parser.add_argument("--deployment-policy", type=Path, required=True)
    parser.add_argument("--actions", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        raw = read_git_record(args.benchmarks, limit=1024 * 1024)
        require(hashlib.sha256(raw).hexdigest() == args.benchmarks_sha256, "managed issuer needs the approved benchmark registrations")
        report = decode(read_git_record(args.observations, limit=256 * 1024))
        require(isinstance(report, dict) and report.get("configuration_sha256") == args.configuration_sha256
                and matches(COMMIT, report.get("source_sha")) and matches(RUN_NUMBER, report.get("workflow_run_id"))
                and matches(RUN_NUMBER, report.get("workflow_run_attempt")) and matches(LABEL, report.get("platform")),
                "managed issuer needs exact row and workflow provenance")
        row = f"{fixture['scope']}/{report['platform']}"
        registrations = decode(raw)
        require(isinstance(registrations, dict) and row in registrations, "no approved benchmark registration for this managed row")
        registration = benchmark_registration(registrations[row])
        policy = decode(read_trace(args.deployment_policy, "deployment_policy"))
        trace = collect_managed_issuer(fixture, report, registration, policy, args.actions)
        with args.output.open("x", encoding="utf-8") as output:
            output.write(json.dumps(trace, sort_keys=True) + "\n")
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
