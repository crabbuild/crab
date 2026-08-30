#!/usr/bin/env python3
"""Verify or compare Crab large-repository RustFS qualification reports."""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


SCHEMA = "crab.large-repository-rustfs"
VERSION = "1.3"
OID_RE = re.compile(r"^[0-9a-f]{40}$")
DIGEST_RE = re.compile(r"^[0-9a-f]{64}$")
SECRET_KEYS = {
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "CRAB_CACHE_PSK",
    "CRAB_CACHE_TOKEN",
}
BASE_REQUIRED_CHECKS = {
    "workspace-volume",
    "source-is-git-repository",
    "replay-commit-count",
    "workspace-free-space",
    "crab-build-matches-source",
    "isolated-remote-prefix",
    "advertised-refs-match-source",
    "clone-tips-match-source",
    "deterministic-object-sample-size",
    "sampled-objects-byte-identical",
    "source-checkout-unchanged",
    "retained-artifacts-redacted",
}
BASE_REQUIRED_STAGES = {
    "initial_import",
    "incremental_seed_clone",
    "full_clone_cold",
    "full_clone_warm",
    "blob_none_clone",
    "depth_1_clone",
    "depth_100_clone",
}
FULL_REQUIRED_STAGES = {
    "depth_10_clone",
    "depth_1000_clone",
}


class VerificationError(RuntimeError):
    """Raised when a report does not prove the qualification contract."""


@dataclass(frozen=True)
class Verification:
    report: Path
    profile: str
    source_revision: str
    replay_count: int
    correctness_fingerprint: str
    operation_summaries: dict[str, dict[str, int]]
    environment_identity: tuple[str, str, str, str, int]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise VerificationError(message)


def load_report(path: Path) -> dict[str, Any]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise VerificationError(f"cannot read report {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise VerificationError(f"report is not valid JSON: {path}: {error}") from error
    require(isinstance(payload, dict), "report root must be an object")
    return payload


def require_nonnegative_int(value: Any, field: str) -> int:
    require(isinstance(value, int) and not isinstance(value, bool), f"{field} must be an integer")
    require(value >= 0, f"{field} must not be negative")
    return value


def reject_secret_values(value: Any, path: str = "report") -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            child_path = f"{path}.{key}"
            if key in SECRET_KEYS:
                require(child in {None, "<redacted>"}, f"{child_path} contains a credential value")
            reject_secret_values(child, child_path)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            reject_secret_values(child, f"{path}[{index}]")


def verify_resources(value: Any, field: str) -> None:
    require(isinstance(value, dict), f"{field} must be an object")
    for name in ("user_cpu_ms", "system_cpu_ms", "children_max_rss"):
        require_nonnegative_int(value.get(name), f"{field}.{name}")
    require(value.get("children_max_rss_unit") == "bytes", f"{field} RSS unit must be bytes")


def verify_telemetry(value: Any, field: str) -> None:
    require(isinstance(value, dict), f"{field} must be an object")
    for name in (
        "storage_requests",
        "storage_bytes",
        "range_get",
        "range_get_coalesced",
        "locator_lookup",
        "cache_hits",
        "cache_misses",
        "logical_objects",
        "inflated_bytes",
        "response_bytes",
        "operation_duration_ms",
        "visibility_duration_ms",
        "upload_pack_duration_ms",
        "visibility_plan_ms",
        "pack_generation_ms",
        "locator_scan",
        "locator_full_scan",
        "locator_exact_fallback",
        "locator_ordinal_scan",
        "locator_ordinal_metadata",
        "locator_ordinal_metadata_scan",
    ):
        require_nonnegative_int(value.get(name), f"{field}.{name}")
    if "source_download_ms" in value:
        require_nonnegative_int(value["source_download_ms"], f"{field}.source_download_ms")


def verify_stage(name: str, stage: Any) -> None:
    require(isinstance(stage, dict), f"stage {name} must be an object")
    require_nonnegative_int(stage.get("duration_ms"), f"stages.{name}.duration_ms")
    if "resources" in stage:
        verify_resources(stage["resources"], f"stages.{name}.resources")
    if "telemetry" in stage:
        verify_telemetry(stage["telemetry"], f"stages.{name}.telemetry")


def verify_full_visibility_telemetry(stages: dict[str, Any]) -> None:
    owner_stage = stages["visibility_owner_seed"]
    owner_telemetry = owner_stage.get("telemetry", {})
    visibility_duration = owner_telemetry.get("visibility_duration_ms", 0)
    owner_actions = owner_stage.get("actions", [])
    if "catalog_visibility_handoff" in owner_actions:
        require(
            isinstance(owner_actions, list)
            and owner_actions
            and owner_actions[-1] == "none",
            "full report catalog visibility handoff did not converge",
        )
        visibility_states = owner_stage.get("visibility_states")
        if visibility_states is not None:
            require(
                isinstance(visibility_states, list)
                and visibility_states
                and visibility_states[-1] == "published",
                "full report catalog visibility handoff did not publish a proof",
            )
        require(
            "visibility_repair" not in owner_actions,
            "full report records a visibility repair beside a catalog handoff",
        )
        return
    if visibility_duration > 0:
        require(
            owner_telemetry.get("storage_requests", 0) > 0
            and owner_telemetry.get("storage_bytes", 0) > 0,
            "full report is missing aggregate visibility storage traffic",
        )
        return
    require(
        isinstance(owner_actions, list)
        and owner_actions
        and owner_actions[-1] == "none",
        "full report is missing visibility-build telemetry and owner convergence",
    )
    visibility_states = owner_stage.get("visibility_states")
    if visibility_states is not None:
        require(
            isinstance(visibility_states, list)
            and visibility_states
            and visibility_states[-1] == "published",
            "full report owner did not finish with a published visibility proof",
        )
    require(
        "visibility_repair" not in owner_actions,
        "full report records a visibility repair without visibility-build telemetry",
    )


def verify_catalog_filter_telemetry(stages: dict[str, Any]) -> None:
    filtered = stages["blob_none_clone"]
    telemetry = filtered.get("telemetry", {})
    require(
        telemetry.get("locator_ordinal_metadata", 0) > 0
        or telemetry.get("locator_ordinal_metadata_scan", 0) > 0,
        "full report did not exercise ordinal metadata for the blobless catalog filter",
    )


def verify_locator_sweep_telemetry(stages: dict[str, Any]) -> None:
    for name, stage in stages.items():
        if not name.startswith("visibility_owner_"):
            continue
        sweeps = stage.get("locator_sweep")
        require(
            isinstance(sweeps, list) and sweeps,
            f"stages.{name} is missing locator sweep telemetry",
        )
        for index, sweep in enumerate(sweeps):
            field = f"stages.{name}.locator_sweep[{index}]"
            require(isinstance(sweep, dict), f"{field} must be an object")
            require(
                isinstance(sweep.get("action"), str) and sweep["action"],
                f"{field}.action is missing",
            )
            for counter in (
                "object_rows_scanned",
                "object_rows_deleted",
                "pack_rows_scanned",
                "pack_rows_deleted",
            ):
                require_nonnegative_int(sweep.get(counter), f"{field}.{counter}")


def verify_team_load(team_load: Any, *, require_release_counts: bool = False) -> None:
    require(isinstance(team_load, dict), "team_load must be an object")
    require(team_load.get("enabled") is True, "team_load is not enabled")
    for field in ("fetch_fanout", "independent_pushes", "contended_pushes"):
        value = require_nonnegative_int(team_load.get(field), f"team_load.{field}")
        require(value > 0, f"team_load.{field} must be positive")
    if require_release_counts:
        require(team_load["fetch_fanout"] == 100, "team load must run 100 concurrent fetches")
        require(team_load["independent_pushes"] == 20, "team load must run 20 independent pushes")
        require(team_load["contended_pushes"] == 20, "team load must run 20 same-ref pushes")

    fetch_seed = team_load.get("fetch_seed")
    require(isinstance(fetch_seed, dict), "team_load.fetch_seed is missing")
    seed_checkpoint = require_nonnegative_int(
        fetch_seed.get("checkpoint"), "team_load.fetch_seed.checkpoint"
    )
    seed_tip = fetch_seed.get("tip")
    require(
        isinstance(seed_tip, str) and OID_RE.fullmatch(seed_tip) is not None,
        "team_load.fetch_seed.tip must be a Git object ID",
    )
    fetch_clients = require_nonnegative_int(
        fetch_seed.get("clients"), "team_load.fetch_seed.clients"
    )
    require(fetch_clients == team_load["fetch_fanout"], "fetch seed fanout mismatch")
    require(
        fetch_seed.get("successful_clones") == fetch_clients,
        "fetch seed clones did not all complete",
    )
    producers = require_nonnegative_int(
        fetch_seed.get("generated_pack_producers"),
        "team_load.fetch_seed.generated_pack_producers",
    )
    require(
        1 <= producers <= 2,
        "cold fetch seed fanout must use one or two generated-pack producers",
    )
    cache_hits = require_nonnegative_int(
        fetch_seed.get("cache_hits"), "team_load.fetch_seed.cache_hits"
    )
    cache_misses = require_nonnegative_int(
        fetch_seed.get("cache_misses"), "team_load.fetch_seed.cache_misses"
    )
    require(
        cache_hits + cache_misses >= fetch_clients,
        "cold fetch seed fanout is missing cache events for clients",
    )
    origin_requests = require_nonnegative_int(
        fetch_seed.get("origin_requests"), "team_load.fetch_seed.origin_requests"
    )
    require(origin_requests > 0, "cold fetch seed fanout recorded no origin requests")
    verify_team_summary(fetch_seed, "team_load.fetch_seed")
    verify_team_results(
        fetch_seed.get("results"),
        fetch_clients,
        "team_load.fetch_seed.results",
        required_category="ok",
    )

    fetch = team_load.get("concurrent_incremental_fetches")
    require(isinstance(fetch, dict), "team_load.concurrent_incremental_fetches is missing")
    from_checkpoint = require_nonnegative_int(
        fetch.get("from_checkpoint"),
        "team_load.concurrent_incremental_fetches.from_checkpoint",
    )
    to_checkpoint = require_nonnegative_int(
        fetch.get("to_checkpoint"),
        "team_load.concurrent_incremental_fetches.to_checkpoint",
    )
    require(
        from_checkpoint == seed_checkpoint and to_checkpoint > from_checkpoint,
        "incremental fetch fanout must span replay commits after its seed",
    )
    require(
        fetch.get("from_tip") == seed_tip
        and isinstance(fetch.get("to_tip"), str)
        and OID_RE.fullmatch(fetch["to_tip"]) is not None
        and fetch["to_tip"] != seed_tip,
        "incremental fetch fanout must move from the seed tip to a different Git object ID",
    )
    require(fetch.get("clients") == fetch_clients, "incremental fetch fanout mismatch")
    require(fetch.get("successful") == fetch_clients, "incremental fetches did not all succeed")
    require(fetch.get("failed") == 0, "incremental fetch failures were recorded")
    verify_team_summary(fetch, "team_load.concurrent_incremental_fetches")
    verify_team_results(
        fetch.get("results"),
        fetch_clients,
        "team_load.concurrent_incremental_fetches.results",
        required_category="ok",
        require_fsck=True,
    )

    independent = team_load.get("independent_ref_pushes")
    require(isinstance(independent, dict), "team_load.independent_ref_pushes is missing")
    independent_clients = team_load["independent_pushes"]
    require(
        independent.get("clients") == independent_clients,
        "independent push fanout mismatch",
    )
    require(
        independent.get("successful") == independent_clients
        and independent.get("rejected") == 0
        and independent.get("unexpected_failures") == 0,
        "independent-ref pushes did not all succeed",
    )
    verify_team_summary(independent, "team_load.independent_ref_pushes")
    verify_team_results(
        independent.get("results"),
        independent_clients,
        "team_load.independent_ref_pushes.results",
        required_category="accepted",
        require_commit=True,
    )

    same_ref = team_load.get("same_ref_pushes")
    require(isinstance(same_ref, dict), "team_load.same_ref_pushes is missing")
    contended_clients = team_load["contended_pushes"]
    require(same_ref.get("clients") == contended_clients, "same-ref push fanout mismatch")
    require(
        same_ref.get("successful") == 1
        and same_ref.get("rejected") == contended_clients - 1
        and same_ref.get("unexpected_failures") == 0,
        "same-ref push outcomes were not one winner plus retryable conflicts",
    )
    verify_team_summary(same_ref, "team_load.same_ref_pushes")
    verify_team_results(
        same_ref.get("results"),
        contended_clients,
        "team_load.same_ref_pushes.results",
        required_category=None,
        allowed_categories={"accepted", "push_lock", "non_fast_forward", "cas_conflict"},
        require_commit=True,
    )


def verify_team_summary(value: dict[str, Any], field: str) -> None:
    for name in ("duration_ms", "median_client_ms", "p95_client_ms", "p99_client_ms"):
        require_nonnegative_int(value.get(name), f"{field}.{name}")
    require(
        value["median_client_ms"]
        <= value["p95_client_ms"]
        <= value["p99_client_ms"],
        f"{field} percentiles are inconsistent",
    )


def verify_team_results(
    results: Any,
    expected_count: int,
    field: str,
    *,
    required_category: str | None,
    allowed_categories: set[str] | None = None,
    require_fsck: bool = False,
    require_commit: bool = False,
) -> None:
    require(isinstance(results, list), f"{field} must be an array")
    require(len(results) == expected_count, f"{field} count mismatch")
    ordinals: list[int] = []
    for index, result in enumerate(results):
        item = f"{field}[{index}]"
        require(isinstance(result, dict), f"{item} must be an object")
        ordinal = require_nonnegative_int(result.get("ordinal"), f"{item}.ordinal")
        ordinals.append(ordinal)
        require_nonnegative_int(result.get("duration_ms"), f"{item}.duration_ms")
        category = result.get("failure_category")
        require(isinstance(category, str) and category, f"{item}.failure_category is missing")
        if required_category is not None:
            require(category == required_category, f"{item} has category {category!r}")
        if allowed_categories is not None:
            require(category in allowed_categories, f"{item} has unexpected category {category!r}")
        exit_code = require_nonnegative_int(result.get("exit_code"), f"{item}.exit_code")
        if category in {"accepted", "ok"}:
            require(exit_code == 0, f"{item} successful category has non-zero exit code")
        else:
            require(exit_code != 0, f"{item} rejected category has zero exit code")
        if require_fsck:
            require(result.get("fetch_exit_code") == 0, f"{item} fetch failed")
            require(result.get("fsck_exit_code") == 0, f"{item} fsck failed")
            require(result.get("tip_matches") is True, f"{item} tip mismatch")
        if require_commit:
            commit = result.get("commit")
            require(
                isinstance(commit, str) and OID_RE.fullmatch(commit),
                f"{item} commit is invalid",
            )
    require(ordinals == list(range(1, expected_count + 1)), f"{field} ordinals are not contiguous")


def verify_cache_service(cache_service: Any) -> None:
    require(isinstance(cache_service, dict), "cache_service must be an object")
    require(cache_service.get("configured") is True, "cache service is not configured")
    require(cache_service.get("required") is True, "cache service was not required")
    require(
        isinstance(cache_service.get("url"), str) and cache_service["url"],
        "cache_service.url is missing",
    )
    require(cache_service.get("health_status") == 200, "cache service health check failed")
    require(
        cache_service.get("capabilities_status") == 200,
        "cache service capabilities check failed",
    )
    require(
        cache_service.get("capabilities_schema") == "crab-cache-service.capabilities.v1",
        "cache service capabilities schema is invalid",
    )
    require(
        cache_service.get("route_schema") == "crab-cache-service.routes.v3",
        "cache service route schema is invalid",
    )

    stats = cache_service.get("stats")
    require(isinstance(stats, dict), "cache_service.stats is missing")
    require(stats.get("status") == 200, "cache service admin stats check failed")
    pack = stats.get("pack")
    require(isinstance(pack, dict), "cache_service.stats.pack is missing")
    for field in (
        "cache_hits",
        "cache_misses",
        "origin_fetches",
        "origin_head_requests",
        "bytes_served_from_cache",
        "bytes_served_from_origin",
        "bytes_served_total",
        "push_warming_writes",
        "push_warming_bytes",
        "read_requests",
    ):
        require_nonnegative_int(pack.get(field), f"cache_service.stats.pack.{field}")
    require(pack["read_requests"] > 0, "cache service recorded no Git pack read traffic")


def verify_report(
    path: Path,
    *,
    allow_smoke: bool = False,
    require_team_load: bool = False,
    require_cache_service: bool = False,
) -> Verification:
    report = load_report(path)
    require(report.get("schema") == SCHEMA, f"unsupported schema: {report.get('schema')!r}")
    require(report.get("version") == VERSION, f"unsupported version: {report.get('version')!r}")
    require(report.get("status") == "ok", f"qualification status is {report.get('status')!r}")
    require(report.get("error") is None, "successful report contains an error")
    require(report.get("valid_for_comparison") is True, "report is marked invalid for comparison")
    require(report.get("started_at"), "report is missing started_at")
    require(report.get("finished_at"), "report is missing finished_at")
    reject_secret_values(report)

    profile = report.get("profile")
    require(profile in {"full", "smoke"}, f"unsupported profile: {profile!r}")
    require(allow_smoke or profile == "full", "smoke report cannot satisfy the full qualification gate")

    source = report.get("source")
    require(isinstance(source, dict), "source must be an object")
    source_revision = source.get("revision")
    base_revision = source.get("base_revision")
    require(isinstance(source_revision, str) and OID_RE.fullmatch(source_revision), "invalid source revision")
    require(isinstance(base_revision, str) and OID_RE.fullmatch(base_revision), "invalid base revision")
    replay_count = require_nonnegative_int(source.get("replay_count"), "source.replay_count")
    require(replay_count >= 1, "replay_count must be positive")
    if profile == "full":
        require(replay_count >= 1_000, "full report must replay at least 1,000 commits")

    provenance = report.get("provenance")
    require(isinstance(provenance, dict), "provenance must be an object")
    for field in (
        "git",
        "crab",
        "aws",
        "python",
        "platform",
        "host",
        "crab_source_revision",
    ):
        require(isinstance(provenance.get(field), str) and provenance[field], f"missing provenance.{field}")
    cpu_count = require_nonnegative_int(provenance.get("cpu_count"), "provenance.cpu_count")
    require(cpu_count > 0, "provenance.cpu_count must be positive")
    object_store = provenance.get("object_store")
    require(isinstance(object_store, dict), "missing provenance.object_store")
    require(object_store.get("kind") == "rustfs", "object store must be RustFS")
    for field in ("endpoint_url", "version"):
        require(
            isinstance(object_store.get(field), str) and object_store[field],
            f"missing provenance.object_store.{field}",
        )
    crab_build = provenance.get("crab_build")
    require(isinstance(crab_build, dict), "missing provenance.crab_build")
    binary_git_sha = crab_build.get("git_sha")
    source_git_sha = provenance["crab_source_revision"]
    require(
        isinstance(binary_git_sha, str)
        and len(binary_git_sha) >= 7
        and binary_git_sha != "unknown"
        and re.fullmatch(r"[0-9a-f]+", binary_git_sha) is not None
        and source_git_sha.startswith(binary_git_sha),
        "Crab binary revision does not match source revision",
    )
    for field in ("crab_version", "git_sha", "build_timestamp"):
        require(
            isinstance(crab_build.get(field), str) and crab_build[field],
            f"missing provenance.crab_build.{field}",
        )
    for field in ("crab_binary_sha256", "harness_sha256", "verifier_sha256"):
        require(
            isinstance(provenance.get(field), str)
            and DIGEST_RE.fullmatch(provenance[field]),
            f"invalid provenance.{field}",
        )

    if require_cache_service:
        verify_cache_service(report.get("cache_service"))

    commands = report.get("commands")
    require(isinstance(commands, list) and commands, "commands must be a non-empty array")
    for index, command in enumerate(commands):
        field = f"commands[{index}]"
        require(isinstance(command, dict), f"{field} must be an object")
        require(isinstance(command.get("name"), str) and command["name"], f"{field}.name is missing")
        require(isinstance(command.get("required_success"), bool), f"{field}.required_success is missing")
        if command["required_success"]:
            require(command.get("exit_code") == 0, f"{field} did not exit successfully")
        else:
            require(isinstance(command.get("exit_code"), int), f"{field}.exit_code must be an integer")
        require_nonnegative_int(command.get("duration_ms"), f"{field}.duration_ms")
        verify_resources(command.get("resources"), f"{field}.resources")
        verify_telemetry(command.get("telemetry"), f"{field}.telemetry")

    checks = report.get("checks")
    require(isinstance(checks, list), "checks must be an array")
    check_names: set[str] = set()
    for index, check in enumerate(checks):
        require(isinstance(check, dict), f"checks[{index}] must be an object")
        name = check.get("name")
        require(isinstance(name, str) and name, f"checks[{index}].name is missing")
        require(name not in check_names, f"duplicate check: {name}")
        check_names.add(name)
        require(check.get("ok") is True, f"check did not pass: {name}")
    required_checks = set(BASE_REQUIRED_CHECKS)
    required_checks.update(
        f"incremental-fetch-tip-{checkpoint}"
        for checkpoint in {1, 10, 100, replay_count}
        if checkpoint <= replay_count
    )
    required_checks.update(
        f"acceleration-current-{checkpoint}"
        for checkpoint in {"seed", 1, 10, 100, replay_count}
        if checkpoint == "seed" or checkpoint <= replay_count
    )
    if require_cache_service:
        required_checks.update(
            {
                "cache-service-configured",
                "cache-service-healthy",
                "cache-service-capabilities",
                "cache-service-admin-stats",
                "cache-service-pack-traffic",
            }
        )
    cleanup = report.get("cleanup")
    require(isinstance(cleanup, dict), "cleanup must be an object")
    require(
        isinstance(cleanup.get("local_worktrees_retained"), bool),
        "cleanup.local_worktrees_retained must be a boolean",
    )
    if cleanup["local_worktrees_retained"]:
        require(
            cleanup.get("local_worktrees_removed") is False,
            "retained local worktrees were marked removed",
        )
    else:
        require(
            cleanup.get("local_worktrees_removed") is True,
            "local worktree cleanup did not complete",
        )
    if cleanup.get("remote_requested"):
        required_checks.add("remote-prefix-cleanup")
        require(
            cleanup.get("remote_completed") is True,
            "requested remote cleanup did not complete",
        )
    missing_checks = sorted(required_checks - check_names)
    require(not missing_checks, f"missing required checks: {missing_checks}")

    stages = report.get("stages")
    require(isinstance(stages, dict), "stages must be an object")
    required_stages = set(BASE_REQUIRED_STAGES)
    if profile == "full":
        required_stages.update(FULL_REQUIRED_STAGES)
    required_stages.update(
        f"incremental_fetch_{checkpoint}"
        for checkpoint in {1, 10, 100, replay_count}
        if checkpoint <= replay_count
    )
    required_stages.update(
        f"pack_inventory_{checkpoint}"
        for checkpoint in {"seed", 1, 10, 100, replay_count}
        if checkpoint == "seed" or checkpoint <= replay_count
    )
    required_stages.update(
        f"{kind}_{checkpoint}"
        for kind in ("visibility_owner", "acceleration")
        for checkpoint in {"seed", 1, 10, 100, replay_count}
        if checkpoint == "seed" or checkpoint <= replay_count
    )
    missing_stages = sorted(required_stages - stages.keys())
    require(not missing_stages, f"missing required stages: {missing_stages}")
    for name, stage in stages.items():
        verify_stage(name, stage)
        if name.startswith("pack_inventory_"):
            require_nonnegative_int(stage.get("active_packs"), f"stages.{name}.active_packs")
            require_nonnegative_int(stage.get("active_pack_bytes"), f"stages.{name}.active_pack_bytes")
        if name.startswith("acceleration_"):
            generation = require_nonnegative_int(
                stage.get("manifest_generation"),
                f"stages.{name}.manifest_generation",
            )
            require(
                isinstance(stage.get("generation_receipt_valid"), bool),
                f"stages.{name}.generation_receipt_valid must be a boolean",
            )
            for field in (
                "ref_registry_repo_complete",
                "locator_available",
                "visibility_available",
            ):
                require(stage.get(field) is True, f"stages.{name}.{field} is not true")
            require(
                stage.get("locator_generation") == generation,
                f"stages.{name} locator generation is stale",
            )
            require(
                stage.get("visibility_generation") == generation,
                f"stages.{name} visibility generation is stale",
            )
            locator_hash = stage.get("locator_pack_index_hash")
            visibility_hash = stage.get("visibility_pack_index_hash")
            require(
                isinstance(locator_hash, str) and DIGEST_RE.fullmatch(locator_hash),
                f"stages.{name} has invalid locator pack-index hash",
            )
            require(
                visibility_hash == locator_hash,
                f"stages.{name} pack-index identities differ",
            )
            require(stage.get("visibility_current") is True, f"stages.{name} visibility is stale")
            require(
                isinstance(stage.get("repair_required"), bool),
                f"stages.{name}.repair_required must be a boolean",
            )
            require(isinstance(stage.get("notes"), list), f"stages.{name}.notes must be an array")
    if profile == "full":
        verify_full_visibility_telemetry(stages)
        verify_catalog_filter_telemetry(stages)
        verify_locator_sweep_telemetry(stages)
        clone_telemetry = stages["full_clone_cold"].get("telemetry", {})
        for field in (
            "upload_pack_duration_ms",
            "visibility_plan_ms",
            "pack_generation_ms",
            "response_bytes",
            "storage_requests",
            "storage_bytes",
        ):
            require(
                clone_telemetry.get(field, 0) > 0,
                f"full report is missing full-clone telemetry: {field}",
            )

    team_load = report.get("team_load")
    if require_team_load:
        verify_team_load(team_load, require_release_counts=True)
        required_team_checks = {
            "concurrent-fetch-seed-clones",
            "concurrent-fetch-seed-generated-pack-producers",
            "concurrent-incremental-fetch-span",
            "concurrent-incremental-fetches",
            "independent_ref_pushes-outcomes",
            "independent-ref-pushes-preserved",
            "same_ref_pushes-outcomes",
            "same-ref-winner-published",
        }
        missing_team_checks = sorted(required_team_checks - check_names)
        require(not missing_team_checks, f"missing team-load checks: {missing_team_checks}")
    elif team_load is not None and isinstance(team_load, dict) and team_load.get("enabled"):
        verify_team_load(team_load)

    pushes = report.get("pushes")
    require(isinstance(pushes, list), "pushes must be an array")
    require(len(pushes) == replay_count + 1, "push count must include seed plus every replay commit")
    require([push.get("ordinal") for push in pushes] == list(range(replay_count + 1)), "push ordinals are not contiguous")
    for index, push in enumerate(pushes):
        require(isinstance(push.get("commit"), str) and OID_RE.fullmatch(push["commit"]), f"push {index} has invalid commit")
        require_nonnegative_int(push.get("duration_ms"), f"pushes[{index}].duration_ms")
        verify_resources(push.get("resources"), f"pushes[{index}].resources")
        verify_telemetry(push.get("telemetry"), f"pushes[{index}].telemetry")

    snapshots = report.get("store_snapshots")
    require(isinstance(snapshots, list) and snapshots, "store_snapshots must be non-empty")
    snapshot_stages = {snapshot.get("stage") for snapshot in snapshots if isinstance(snapshot, dict)}
    required_snapshot_stages = {
        str(checkpoint)
        for checkpoint in {"seed", 1, 10, 100, replay_count, "final"}
        if isinstance(checkpoint, str) or checkpoint <= replay_count
    }
    missing_snapshots = sorted(required_snapshot_stages - snapshot_stages)
    require(not missing_snapshots, f"missing store snapshots: {missing_snapshots}")
    for index, snapshot in enumerate(snapshots):
        require(isinstance(snapshot, dict), f"store_snapshots[{index}] must be an object")
        for field in ("objects", "bytes", "physical_packs", "physical_pack_bytes"):
            require_nonnegative_int(snapshot.get(field), f"store_snapshots[{index}].{field}")

    correctness = report.get("correctness")
    require(isinstance(correctness, dict), "correctness must be an object")
    fingerprint = correctness.get("fingerprint")
    require(isinstance(fingerprint, str) and DIGEST_RE.fullmatch(fingerprint), "invalid correctness fingerprint")
    require(correctness.get("source_head") == source_revision, "correctness source head mismatch")
    require(correctness.get("full_clone_head") == source_revision, "full clone head mismatch")
    require(correctness.get("incremental_clone_head") == source_revision, "incremental clone head mismatch")
    require(correctness.get("full_fsck") is True, "full clone fsck evidence is missing")
    require(correctness.get("incremental_fsck") is True, "incremental clone fsck evidence is missing")
    sample_size = require_nonnegative_int(correctness.get("sample_size"), "correctness.sample_size")
    require(sample_size >= 1, "correctness sample must not be empty")
    if profile == "full":
        require(sample_size >= 1_000, "full report must verify at least 1,000 objects")
    refs = correctness.get("advertised_refs")
    require(isinstance(refs, dict), "advertised_refs must be an object")
    require(refs.get("refs/heads/main") == source_revision, "advertised main ref mismatch")
    for name, oid in refs.items():
        require(
            isinstance(name, str)
            and (name == "HEAD" or name.startswith("refs/")),
            f"invalid advertised ref name: {name!r}",
        )
        require(
            isinstance(oid, str) and OID_RE.fullmatch(oid),
            f"invalid advertised ref oid for {name}",
        )
    clone_refs = correctness.get("clone_refs")
    require(isinstance(clone_refs, dict), "clone_refs must be an object")
    require(clone_refs == refs, "clone advertised refs do not match remote advertisement")

    metrics = report.get("metrics")
    require(isinstance(metrics, dict), "metrics must be an object")
    require(metrics.get("replay_pushes") == replay_count, "metrics replay push count mismatch")
    require(metrics.get("total_pushes") == replay_count + 1, "metrics total push count mismatch")
    summaries = metrics.get("operation_summaries")
    require(isinstance(summaries, dict), "operation summaries must be an object")
    for required in ("push", "clone", "fetch"):
        require(required in summaries, f"missing operation summary: {required}")
    normalized: dict[str, dict[str, int]] = {}
    for name, summary in summaries.items():
        require(isinstance(summary, dict), f"operation summary {name} must be an object")
        normalized[name] = {
            field: require_nonnegative_int(summary.get(field), f"operation summary {name}.{field}")
            for field in ("count", "min_ms", "median_ms", "p95_ms", "p99_ms", "max_ms")
        }
        require(normalized[name]["count"] > 0, f"operation summary {name} is empty")
        require(
            normalized[name]["min_ms"]
            <= normalized[name]["median_ms"]
            <= normalized[name]["p95_ms"]
            <= normalized[name]["p99_ms"]
            <= normalized[name]["max_ms"],
            f"operation summary {name} percentiles are inconsistent",
        )

    return Verification(
        report=path,
        profile=profile,
        source_revision=source_revision,
        replay_count=replay_count,
        correctness_fingerprint=fingerprint,
        operation_summaries=normalized,
        environment_identity=(
            provenance["host"],
            provenance["platform"],
            provenance["git"],
            provenance["crab"],
            cpu_count,
        ),
    )


def compare_reports(
    baseline_path: Path,
    candidate_path: Path,
    *,
    maximum_drift: float,
    allow_smoke: bool,
    require_team_load: bool = False,
    require_cache_service: bool = False,
) -> dict[str, Any]:
    require(math.isfinite(maximum_drift) and maximum_drift >= 0, "maximum drift must be non-negative")
    baseline = verify_report(
        baseline_path,
        allow_smoke=allow_smoke,
        require_team_load=require_team_load,
        require_cache_service=require_cache_service,
    )
    candidate = verify_report(
        candidate_path,
        allow_smoke=allow_smoke,
        require_team_load=require_team_load,
        require_cache_service=require_cache_service,
    )
    require(baseline.profile == candidate.profile, "report profiles differ")
    require(baseline.source_revision == candidate.source_revision, "source revisions differ")
    require(baseline.replay_count == candidate.replay_count, "replay counts differ")
    require(
        baseline.environment_identity == candidate.environment_identity,
        "host or toolchain identity differs",
    )
    require(
        baseline.correctness_fingerprint == candidate.correctness_fingerprint,
        "correctness fingerprints differ",
    )
    comparisons: dict[str, Any] = {}
    for operation in ("push", "clone", "fetch"):
        baseline_median = baseline.operation_summaries[operation]["median_ms"]
        candidate_median = candidate.operation_summaries[operation]["median_ms"]
        if baseline_median == 0:
            drift = 0.0 if candidate_median == 0 else math.inf
        else:
            drift = abs(candidate_median - baseline_median) / baseline_median
        comparisons[operation] = {
            "baseline_median_ms": baseline_median,
            "candidate_median_ms": candidate_median,
            "absolute_drift_ratio": drift,
            "within_limit": drift <= maximum_drift,
        }
    require(comparisons, "reports have no comparable operation summaries")
    failures = [name for name, value in comparisons.items() if not value["within_limit"]]
    return {
        "schema": "crab.large-repository-rustfs-comparison",
        "version": "1.0",
        "status": "ok" if not failures else "invalid",
        "valid_for_comparison": not failures,
        "comparison_invalid_reason": (
            None
            if not failures
            else f"host contention or instability: median drift exceeds limit for {failures}"
        ),
        "baseline": str(baseline_path),
        "candidate": str(candidate_path),
        "maximum_drift_ratio": maximum_drift,
        "source_revision": baseline.source_revision,
        "correctness_fingerprint": baseline.correctness_fingerprint,
        "operations": comparisons,
    }


def write_output(payload: dict[str, Any], path: Path | None) -> None:
    body = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    if path is None:
        print(body, end="")
    else:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command")

    verify = subparsers.add_parser("verify", help="verify one report")
    verify.add_argument("report", type=Path)
    verify.add_argument("--allow-smoke", action="store_true")
    verify.add_argument("--require-team-load", action="store_true")
    verify.add_argument("--require-cache-service", action="store_true")
    verify.add_argument("--output", type=Path)

    compare = subparsers.add_parser("compare", help="verify and compare two reports")
    compare.add_argument("baseline", type=Path)
    compare.add_argument("candidate", type=Path)
    compare.add_argument("--maximum-drift", type=float, default=0.20)
    compare.add_argument("--allow-smoke", action="store_true")
    compare.add_argument("--require-team-load", action="store_true")
    compare.add_argument("--require-cache-service", action="store_true")
    compare.add_argument("--output", type=Path)

    args = parser.parse_args()
    return args


def main() -> int:
    args = parse_args()
    try:
        if args.command == "compare":
            payload = compare_reports(
                args.baseline,
                args.candidate,
                maximum_drift=args.maximum_drift,
                allow_smoke=args.allow_smoke,
                require_team_load=args.require_team_load,
                require_cache_service=args.require_cache_service,
            )
            write_output(payload, args.output)
            if payload["status"] != "ok":
                print(payload["comparison_invalid_reason"], file=sys.stderr)
                return 1
        else:
            verification = verify_report(
                args.report,
                allow_smoke=getattr(args, "allow_smoke", False),
                require_team_load=getattr(args, "require_team_load", False),
                require_cache_service=getattr(args, "require_cache_service", False),
            )
            payload = {
                "schema": "crab.large-repository-rustfs-verification",
                "version": "1.0",
                "status": "ok",
                "report": str(verification.report),
                "profile": verification.profile,
                "source_revision": verification.source_revision,
                "replay_count": verification.replay_count,
                "correctness_fingerprint": verification.correctness_fingerprint,
            }
            write_output(payload, getattr(args, "output", None))
        return 0
    except VerificationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] not in {"verify", "compare", "-h", "--help"}:
        sys.argv.insert(1, "verify")
    raise SystemExit(main())
