#!/usr/bin/env python3
"""Assemble and verify a release artifact from measured CDN qualification inputs."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path
from tempfile import TemporaryDirectory

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cdn_qualification import (  # noqa: E402
    CACHE_REUSE_CHECK, CDN_PROVENANCE_FIELDS, COMMIT, EXPIRY_CHECKS, GIT_BENCHMARK_DIR, GIT_PRODUCT_DIR, LABEL, PROVIDERS,
    PROVIDER_FOR_EDGE, PUBLIC_EXPORT_BEFORE, PUBLIC_EXPORT_CHECK, PUBLIC_EXPORT_DIR, PUBLIC_EXPORT_FILES,
    PUBLIC_TRANSITION_CHECK, RUN_NUMBER, SCHEMA, SCOPED_VIEW_CHECK, SERVICE_PRODUCT_DIR, SHA256, UNRELATED_VIEWER_CHECK,
    benchmark_registration, collect_desktop_product_action, collect_git_performance, collect_git_product_action,
    collect_xorb_read_evidence, collect_sdk_product_action, collect_service_product_action, decode,
    derive_git_blob_public_transition, derive_service_public_transition, fields,
    is_direct_sdk_git_scope, is_direct_xorb_read_scope, is_direct_service_scope,
    matches, read_trace, release_scopes, require,
    is_managed_cli_git_scope, required_check_names, required_platform_rows, requires_cli_workload, requires_desktop_workload,
    scope_parts, verify_bundle_row,
    verify_cleanup_trace, verify_deployment_policy_trace,
)
from public_inventory import ExportError, read_document


POLICY = Path(__file__).resolve().parents[3] / "crates/crab-storage/src/cdn/qualified-scopes.txt"
OBSERVATION_FIELDS = {
    "schema", "source_sha", "scope", "configuration_sha256", "workflow_run_id", "workflow_run_attempt",
    "platform", "deployment", "fixture_prefix", "check_results", "product_action", "performance",
}
CANARY_COMMON = {
    "deployment_policy", "exact_get", "exact_range", "edge_route_policy", "mutable_route_denied", "neighbor_route_denied",
    "native_origin_read",
}
CANARY_REQUIRED = {
    "private": CANARY_COMMON | {"warm_anonymous_viewer_denied", "private_origin_denied"},
    "public": CANARY_COMMON | {PUBLIC_EXPORT_CHECK, "live_origin_denied", "export_origin_access", "public_listing_denied"},
}
CANARY_OPTIONAL = {
    "private": {CACHE_REUSE_CHECK, UNRELATED_VIEWER_CHECK} | EXPIRY_CHECKS,
    "public": set(),
}


def load_canary_traces(root: Path, source: str, run_id: str, attempt: str) -> dict[tuple[str, str], dict[str, bytes]]:
    require(root.is_dir() and not root.is_symlink(), "CDN canary trace root must be a directory")
    batches = {}
    for directory in sorted(root.iterdir()):
        provider = directory.name
        require(provider in PROVIDERS and directory.is_dir() and not directory.is_symlink(), "unexpected provider canary directory")
        policy_path = directory / "deployment_policy.json"
        require(policy_path.is_file() and not policy_path.is_symlink(), "canary needs a deployment policy trace")
        policy_raw = read_trace(policy_path, "deployment_policy")
        policy = decode(policy_raw)
        require(isinstance(policy, dict) and isinstance(policy.get("scope"), str)
                and matches(SHA256, policy.get("configuration_sha256")), "invalid CDN canary scope or configuration")
        scope, configuration = policy["scope"], policy["configuration_sha256"]
        edge, access, _, _, _ = scope_parts(scope)
        require(PROVIDER_FOR_EDGE[edge] == provider, "canary scope does not match its provider directory")
        provenance = {"source_sha": source, "scope": scope, "configuration_sha256": configuration,
                      "workflow_run_id": run_id, "workflow_run_attempt": attempt}
        verify_deployment_policy_trace(policy, provenance, edge, access, policy.get("deployment_policy_sha256"))
        traces = {"deployment_policy": policy_raw}
        approved_names = CANARY_REQUIRED[access] | CANARY_OPTIONAL[access]
        for path in sorted(directory.iterdir()):
            name = path.stem
            require(path.suffix == ".json" and name in approved_names and path.is_file() and not path.is_symlink(),
                    "unexpected CDN canary trace file")
            if name == "deployment_policy":
                continue
            raw = read_trace(path, name)
            trace = decode(raw)
            schema = "crab.cdn-authenticated-cache-reuse/v1" if name == CACHE_REUSE_CHECK else "crab.cdn-trace/v1"
            require(isinstance(trace, dict) and trace.get("schema") == schema and trace.get("check") == name
                    and all(trace.get(key) == provenance[key] for key in CDN_PROVENANCE_FIELDS),
                    f"canary trace provenance mismatch: {name}")
            traces[name] = raw
        require(CANARY_REQUIRED[access] <= traces.keys(), "CDN canary artifact is missing required transport observations")
        require(not EXPIRY_CHECKS & traces.keys() or EXPIRY_CHECKS <= traces.keys(),
                "CDN canary artifact must include both expiry observations together")
        batches[(scope, configuration)] = traces
    require(bool(batches), "CDN canary trace root is empty")
    return batches


def assemble_bundle(
    inputs: Path, output: Path, benchmarks_sha256: str, scopes: set[str],
    source: str, run_id: str, attempt: str,
    *, native_reports: Path | None = None, transport_traces: Path | None = None, policy: Path | None = None,
) -> int:
    require(not output.exists() and not output.is_symlink(), "qualification artifact directory must be new")
    require(matches(COMMIT, source) and matches(RUN_NUMBER, run_id) and matches(RUN_NUMBER, attempt),
            "exact workflow run provenance is required")
    raw = (inputs / "benchmarks.json").read_bytes()
    require(hashlib.sha256(raw).hexdigest() == benchmarks_sha256,
            "CDN benchmark registrations differ from the approved digest")
    registrations = decode(raw)
    require(isinstance(registrations, dict) and registrations, "CDN artifact needs benchmark registrations")
    release_rows = required_platform_rows(scopes)
    missing = release_rows - registrations.keys()
    require(not missing, f"missing required CDN platform registrations: {', '.join(sorted(missing))}")
    origin_inputs = inputs if native_reports is None else native_reports
    canaries = load_canary_traces(transport_traces, source, run_id, attempt) if transport_traces is not None else {}

    # Stage only the evidence files that pass verification. A failed record
    # must never leave a publishable bundle at the artifact path.
    with TemporaryDirectory(prefix=".crab-cdn-artifact-", dir=output.parent) as temporary:
        stage = Path(temporary)
        (stage / "benchmarks.json").write_bytes(raw)
        for row, registration in sorted(registrations.items()):
            scope, _, platform = row.rpartition("/")
            edge, access, _, _, profile = scope_parts(scope)
            require(matches(LABEL, platform), "invalid CDN qualification platform")
            provider = PROVIDER_FOR_EDGE[edge]
            native_report = stage / provider / "report.json"
            if not native_report.exists():
                native_report.parent.mkdir()
                native_report.write_bytes((origin_inputs / provider / "report.json").read_bytes())

            cli_workload = requires_cli_workload(scope)
            managed_git = is_managed_cli_git_scope(scope)
            sdk_workload = is_direct_sdk_git_scope(scope)
            xorb_read = is_direct_xorb_read_scope(scope)
            require(isinstance(registration, dict) and matches(SHA256, registration.get("configuration_sha256")),
                    f"invalid CDN configuration digest for {row}")
            desktop = requires_desktop_workload(scope, registration.get("workload"))
            service = is_direct_service_scope(scope)
            expected = OBSERVATION_FIELDS.copy()
            if cli_workload or sdk_workload or xorb_read or desktop or service:
                expected.difference_update({"product_action", "performance"})
            measured = fields(
                decode((inputs / row / "observations.json").read_bytes()),
                expected, "CDN observations",
            )
            require(measured["schema"] == "crab.cdn-observations/v1", "invalid CDN observations schema")
            names = required_check_names(access, profile)
            spec = benchmark_registration(registration) if cli_workload or sdk_workload or xorb_read or desktop or service else registration
            canary = canaries.get((scope, registration["configuration_sha256"]), {})
            outcomes = measured.pop("check_results")
            require(isinstance(outcomes, dict) and names - canary.keys() <= outcomes.keys() <= names,
                    "invalid fields in measured check results")
            destination = stage / row
            (destination / "traces").mkdir(parents=True)
            if access == "public":
                publication = inputs / row / PUBLIC_EXPORT_DIR
                retained = destination / PUBLIC_EXPORT_DIR
                for version in ("", PUBLIC_EXPORT_BEFORE):
                    origin, target = publication / version, retained / version
                    require(origin.is_dir() and not origin.is_symlink(), f"{row} needs the public export publication inputs")
                    target.mkdir()
                    for name in PUBLIC_EXPORT_FILES:
                        path = origin / name
                        require(path.is_file() and not path.is_symlink(), f"{row} needs a regular publication artifact")
                        raw, _ = read_document(path)
                        (target / name).write_bytes(raw)
            checks = {}
            for check in sorted(names):
                filename = f"{check}.json"
                trace_path = inputs / row / "traces" / filename
                require(outcomes.get(check, True) is True, f"{row} failed measured check: {check}")
                if check == PUBLIC_TRANSITION_CHECK and (sdk_workload or desktop or service):
                    if service:
                        transition = derive_service_public_transition(inputs / row / SERVICE_PRODUCT_DIR, measured, spec)
                    else:
                        transition = derive_git_blob_public_transition(inputs / row / GIT_PRODUCT_DIR, measured, spec)
                    if trace_path.exists():
                        require(decode(read_trace(trace_path, check)) == transition,
                                "measured public transition differs from the retained product reads")
                    trace = (json.dumps(transition, sort_keys=True) + "\n").encode()
                elif check in canary:
                    trace = canary[check]
                    if trace_path.exists():
                        require(read_trace(trace_path, check) == trace, f"measured trace conflicts with same-attempt canary: {check}")
                else:
                    trace = read_trace(trace_path, check)
                (destination / "traces" / filename).write_bytes(trace)
                checks[check] = {"passed": True, "trace_sha256": hashlib.sha256(trace).hexdigest()}
            native_target = None
            product_request_ids = None
            desktop_product = None
            if cli_workload or sdk_workload or xorb_read or desktop or service:
                cleanup = decode(read_trace(destination / "traces/cleanup.json", "cleanup"))
                cleanup_at = verify_cleanup_trace(cleanup, measured, edge, access)
                origin = cleanup["targets"]["native"]["origin_sha256"]
                rollback = read_trace(destination / "traces/rollback.json", "rollback")
            if cli_workload or sdk_workload or desktop:
                native_target = {"origin_sha256": origin, "prefix_sha256": cleanup["prefix_sha256"]}
            if cli_workload:
                registered_read = {key: registration.get(key) for key in ("candidate", "viewer_binding_sha256")}
                if managed_git:
                    registered_read["managed_issuer"] = spec["managed_issuer"]
                product = collect_git_product_action(
                    inputs / row / GIT_PRODUCT_DIR, measured | registered_read, rollback,
                    retain=destination / GIT_PRODUCT_DIR, cleanup_at=cleanup_at, native_origin_sha256=origin,
                    public_export_sha256=registration.get("publication_sha256", ""),
                )
                measured["product_action"] = product | {"fixture_cleanup_verified": True}
            elif sdk_workload:
                measured["product_action"], product_request_ids = collect_sdk_product_action(
                    inputs / row / GIT_PRODUCT_DIR, measured, spec, rollback,
                    cleanup_at=cleanup_at, native_origin_sha256=origin,
                    retain=destination / GIT_PRODUCT_DIR,
                )
            elif xorb_read:
                placement = decode(read_trace(destination / "traces" / f"{SCOPED_VIEW_CHECK}.json",
                                              SCOPED_VIEW_CHECK))["placement"]
                measured["product_action"], measured["performance"] = collect_xorb_read_evidence(
                    inputs / row, measured, spec, rollback, placement=placement,
                    cleanup_at=cleanup_at, native_origin_sha256=origin,
                    retain=destination,
                )
            elif desktop:
                desktop_product = collect_desktop_product_action(
                    inputs / row / GIT_PRODUCT_DIR, measured, spec, rollback,
                    cleanup_at=cleanup_at, native_origin_sha256=origin, retain=destination / GIT_PRODUCT_DIR,
                )
                measured["product_action"] = desktop_product.action
            elif service:
                measured["product_action"] = collect_service_product_action(
                    inputs / row / SERVICE_PRODUCT_DIR, measured, spec, rollback,
                    cleanup_at=cleanup_at, native_origin_sha256=origin,
                    retain=destination / SERVICE_PRODUCT_DIR,
                )
            if cli_workload or sdk_workload or desktop or service:
                measured["performance"] = collect_git_performance(
                    inputs / row / GIT_BENCHMARK_DIR, registration, platform, run_id, attempt,
                    retain=destination / GIT_BENCHMARK_DIR, native_target=native_target,
                    service_row=destination if service else None, product_request_ids=product_request_ids,
                    desktop_product=desktop_product, managed_row=destination if managed_git else None,
                )
            report = measured | {
                "schema": SCHEMA,
                "schema_version": 1,
                "status": "qualified",
                "origin_report_sha256": hashlib.sha256(native_report.read_bytes()).hexdigest(),
                "checks": checks,
            }
            (destination / "cdn-report.json").write_text(
                json.dumps(report, sort_keys=True) + "\n", encoding="utf-8",
            )
            verify_bundle_row(stage, row, registration, source, run_id, attempt, policy=policy if row in release_rows else None)
        require(not output.exists() and not output.is_symlink(), "qualification artifact directory already exists")
        stage.rename(output)
    return len(registrations)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, required=True)
    parser.add_argument("--native-reports", type=Path, help="read native provider reports from this separate directory")
    parser.add_argument("--transport-traces", type=Path, help="read same-attempt canary artifacts from provider subdirectories")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--benchmarks-sha256", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    args = parser.parse_args()
    try:
        count = assemble_bundle(
            args.inputs, args.output, args.benchmarks_sha256, release_scopes(POLICY),
            args.source_sha, args.run_id, args.run_attempt,
            native_reports=args.native_reports, transport_traces=args.transport_traces, policy=POLICY,
        )
    except subprocess.CalledProcessError:
        print("error: assembled CDN report did not pass verification", file=sys.stderr)
        return 1
    except (ExportError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"Assembled {count} verified CDN qualification reports")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
