#!/usr/bin/env python3
"""Regression tests for cache-service smoke report verification."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from collections.abc import Callable
from hashlib import sha256
from pathlib import Path
from typing import Any


SCRIPT = Path(__file__).with_name("verify-cache-service-smoke-report.py")
SMOKE_SCRIPT = Path(__file__).parent / "e2e" / "run_cache_service_rustfs_smoke.py"
RUN_ID = "fixture-run"
MANIFEST_KEY = f"e2e-cache-service/{RUN_ID}/cli-dedup/manifest"
DEFAULT_PSK_BLAKE3 = "4fb898757c4c93662343bbbb25419f8c4f9c979352d40ff896578cabf620cf6e"
EVIDENCE_MANIFEST_SCHEMA = "crab-cache-service.evidence-manifest.v1"
SHA256 = "a" * 64
REDACTED_CONFIG = """[server]
listen_addr = "127.0.0.1:0"
mutable_path_mode = "strict"
policy_path = "policy.yaml"
drain_timeout_secs = 1

[auth]
mechanism = "psk"
psk_hash = "<redacted>"

[origin]
url = "s3://crab"

[cache]
root = "/tmp/cache"
max_bytes = 16777216

[dedup]
scope = "all"

[eviction]
high_water_ratio = 0.95
low_water_ratio = 0.90
"""
REDACTED_POLICY = """rules:
  - principal: "<redacted>"
    repos: ["<run-scope>", ".crab"]
    actions: ["read", "write", "dedup", "admin"]
"""


def preflight_payload() -> dict[str, Any]:
    return {
        "status": "warn",
        "summary": {
            "auth": "psk",
            "max_object_bytes": 1048576,
            "mutable_path_mode": "strict",
            "policy": "configured",
            "policy_diagnostics": {
                "rule_count": 1,
                "repo_pattern_count": 2,
                "actions": ["read", "write", "dedup", "admin"],
            },
            "tls": "plain_http",
        },
        "checks": [
            {
                "name": "startup",
                "status": "ok",
            },
            {
                "name": "origin",
                "status": "ok",
            },
            {
                "name": "authorization policy",
                "status": "ok",
            },
            {
                "name": "enterprise profile",
                "status": "ok",
            },
            {
                "name": "tls",
                "status": "warn",
                "code": "tls_not_configured",
            },
        ],
    }


def embedded_checks() -> list[dict[str, Any]]:
    names = [
        "cache-server-preflight-no-failures",
        "cache-server-preflight-startup-ok",
        "cache-server-preflight-origin-ok",
        "cache-server-preflight-policy-loaded",
        "cache-server-preflight-enterprise-profile-ok",
        "cache-server-preflight-policy-diagnostics",
        "cache-server-preflight-no-enterprise-profile-failures",
        "cache-server-preflight-secret-redacted",
        "enterprise-onboarding-rendered",
        "enterprise-onboarding-check-ok",
        "enterprise-onboarding-policy-path-wired",
        "enterprise-onboarding-client-config-cache-dedup",
        "enterprise-onboarding-client-env-secret-manager-placeholder",
        "enterprise-onboarding-check-secret-redacted",
        "enterprise-onboarding-probe-ok-or-warn",
        "enterprise-onboarding-probe-secret-redacted",
        "enterprise-onboarding-client-probe-ok",
        "enterprise-onboarding-client-probe-secret-redacted",
        "cache-server-health",
        "cache-server-origin-health-did-not-get-object",
        "doctor-cache-service-caps-ok",
        "doctor-cache-service-authz-ok",
        "doctor-cache-service-health-ok",
        "doctor-cache-service-auth-ok",
        "doctor-cache-service-admin-ok",
        "doctor-cache-service-active-secret-redacted",
        "cli-dedup-push-skipped-cacheable-origin-get",
        "cli-dedup-push-only-origin-manifest-cas-read",
        "cli-dedup-push-cache-service-mutable-rejections-flat",
        "cli-hydrates-use-push-warmed-cache-service",
        "post-traffic-support-bundle-metrics-origin-avoidance",
        "cache-server-still-running",
    ]
    checks = [{"name": name, "ok": True, "detail": {}} for name in names]
    checks.append(
        {
            "name": "doctor-cache-service-active-ok",
            "ok": True,
            "detail": {"cleanup": "cleanup ok"},
        }
    )
    return checks


def auth_control_records() -> list[dict[str, Any]]:
    return [
        {
            "name": "auth-missing-psk-rejected",
            "status": 401,
            "origin_gets_before": 0,
            "origin_gets_after": 0,
            "cache_status": "",
        },
        {
            "name": "auth-wrong-psk-rejected",
            "status": 401,
            "origin_gets_before": 0,
            "origin_gets_after": 0,
            "cache_status": "",
        },
        {
            "name": "auth-valid-psk-accepted",
            "status": 200,
            "origin_gets_before": 0,
            "origin_gets_after": 1,
            "cache_status": "MISS",
            "body_len": 4096,
        },
        {
            "name": "auth-policy-denies-out-of-scope-read",
            "status": 403,
            "origin_gets_before": 1,
            "origin_gets_after": 1,
            "cache_status": "",
        },
    ]


def transparent_mutable_records() -> list[dict[str, Any]]:
    return [
        {
            "name": "transparent-mutable-allowed-get",
            "status": 200,
            "origin_gets_before": 0,
            "origin_gets_after": 1,
            "origin_heads_before": 0,
            "origin_heads_after": 0,
            "mutable_proxy_reads_before": 0,
            "mutable_proxy_reads_after": 1,
            "body_len": 4096,
        },
        {
            "name": "transparent-mutable-denied-get",
            "status": 403,
            "origin_gets_before": 1,
            "origin_gets_after": 1,
            "origin_heads_before": 0,
            "origin_heads_after": 0,
            "mutable_proxy_reads_before": 1,
            "mutable_proxy_reads_after": 1,
            "body_len": 0,
        },
        {
            "name": "transparent-mutable-denied-head",
            "status": 403,
            "origin_gets_before": 1,
            "origin_gets_after": 1,
            "origin_heads_before": 0,
            "origin_heads_after": 0,
            "mutable_proxy_reads_before": 1,
            "mutable_proxy_reads_after": 1,
            "body_len": 0,
        },
        {
            "name": "transparent-mutable-ambiguous-get",
            "status": 400,
            "origin_gets_before": 1,
            "origin_gets_after": 1,
            "origin_heads_before": 0,
            "origin_heads_after": 0,
            "mutable_proxy_reads_before": 1,
            "mutable_proxy_reads_after": 1,
            "body_len": 0,
        },
    ]


def read_records() -> list[dict[str, Any]]:
    return [
        {
            "name": "full-first-miss",
            "key": ".crab/xorbs/a",
            "status": 200,
            "cache_status": "MISS",
            "origin_gets_for_key": 1,
            "body_len": 4096,
        },
        {
            "name": "full-second-hit",
            "key": ".crab/xorbs/a",
            "status": 200,
            "cache_status": "HIT",
            "origin_gets_for_key": 1,
            "body_len": 4096,
        },
        {
            "name": "warm-range-hit",
            "key": ".crab/xorbs/a",
            "status": 206,
            "cache_status": "HIT",
            "origin_gets_for_key": 1,
            "body_len": 128,
        },
        {
            "name": "cold-range-first-miss",
            "key": ".crab/xorbs/b",
            "status": 206,
            "cache_status": "MISS",
            "origin_gets_for_key": 1,
            "body_len": 128,
        },
        {
            "name": "cold-range-second-hit",
            "key": ".crab/xorbs/b",
            "status": 206,
            "cache_status": "HIT",
            "origin_gets_for_key": 1,
            "body_len": 128,
        },
    ]


def immutable_route_behavior_records() -> list[dict[str, Any]]:
    return [
        {
            "name": f"route-contract-immutable-{idx}",
            "pattern": pattern,
            "key": f"route-contract/immutable/{idx}",
            "first_status": 200,
            "first_cache_status": "MISS" if idx == 0 else "HIT",
            "second_status": 200,
            "second_cache_status": "HIT",
            "range_status": 206,
            "range_cache_status": "HIT",
            "origin_gets_before": 0,
            "origin_gets_after_first": 1 if idx == 0 else 0,
            "origin_gets_after_second": 1 if idx == 0 else 0,
            "origin_gets_after_range": 1 if idx == 0 else 0,
            "body_len": 4096,
            "range_body_len": 3,
        }
        for idx, pattern in enumerate(
            [
                ".crab/xorbs/{hash}",
                ".crab/shards/{hash}",
                "{repo}/packs/pack-{id}.pack",
                "{repo}/packs/pack-{id}.idx",
                "{repo}/file_index_db/compacted/*.sst",
                "{repo}/file_index_db/manifest/*.manifest",
                "{repo}/file_index_db/wal/*.sst",
                "{repo}/file_index_db/compactions/*.compactions",
                ".crab/chunk_index_db/compacted/*.sst",
                ".crab/chunk_index_db/manifest/*.manifest",
                ".crab/chunk_index_db/wal/*.sst",
                ".crab/chunk_index_db/compactions/*.compactions",
            ]
        )
    ]


def immutable_route_write_behavior_records() -> list[dict[str, Any]]:
    return [
        {
            "name": f"route-contract-immutable-write-{idx}",
            "pattern": pattern,
            "key": f"route-contract/immutable/{idx}",
            "put_status": 201,
            "put_cache_status": "",
            "get_status": 200,
            "get_cache_status": "HIT",
            "head_status": 200,
            "head_cache_status": "HIT",
            "range_status": 206,
            "range_cache_status": "HIT",
            "evict_status": 200,
            "origin_gets_before": 0,
            "origin_gets_after_put": 0,
            "origin_gets_after_get": 0,
            "origin_gets_after_head": 0,
            "origin_gets_after_range": 0,
            "origin_puts_before": 0,
            "origin_puts_after": 0,
            "total_origin_gets_before": 11,
            "total_origin_gets_after": 11,
            "total_origin_puts_before": 13,
            "total_origin_puts_after": 13,
            "total_bytes_before": 4096,
            "total_bytes_after": 8192,
            "push_warming_writes_before": 1,
            "push_warming_writes_after": 2,
            "push_warming_bytes_before": 1024,
            "push_warming_bytes_after": 5120,
            "body_len": 4096,
            "get_body_len": 4096,
            "range_body_len": 3,
        }
        for idx, pattern in enumerate(
            [
                ".crab/xorbs/{hash}",
                ".crab/shards/{hash}",
                "{repo}/packs/pack-{id}.pack",
                "{repo}/packs/pack-{id}.idx",
                "{repo}/file_index_db/compacted/*.sst",
                "{repo}/file_index_db/manifest/*.manifest",
                "{repo}/file_index_db/wal/*.sst",
                "{repo}/file_index_db/compactions/*.compactions",
                ".crab/chunk_index_db/compacted/*.sst",
                ".crab/chunk_index_db/manifest/*.manifest",
                ".crab/chunk_index_db/wal/*.sst",
                ".crab/chunk_index_db/compactions/*.compactions",
            ]
        )
    ]


def immutable_poisoning_control_records() -> list[dict[str, Any]]:
    return [
        {
            "name": f"route-contract-immutable-poison-{idx}",
            "pattern": pattern,
            "key": f"route-contract/immutable-poison/{idx}",
            "corrupt_status": 409,
            "corrupt_cache_status": "",
            "recovery_status": 200,
            "recovery_cache_status": "MISS",
            "second_status": 200,
            "second_cache_status": "HIT",
            "evict_status": 200,
            "origin_gets_before": 0,
            "origin_gets_after_reject": 0,
            "origin_gets_after_recovery": 1,
            "origin_gets_after_second": 1,
            "origin_puts_before": 0,
            "origin_puts_after": 0,
            "total_origin_gets_before": 11,
            "total_origin_gets_after_reject": 11,
            "total_origin_gets_after_recovery": 12,
            "total_origin_gets_after_second": 12,
            "total_origin_puts_before": 13,
            "total_origin_puts_after": 13,
            "total_bytes_before": 4096,
            "total_bytes_after_reject": 4096,
            "total_bytes_after_recovery": 8192,
            "push_warming_writes_before": 2,
            "push_warming_writes_after_reject": 2,
            "push_warming_writes_after_recovery": 2,
            "push_warming_bytes_before": 5120,
            "push_warming_bytes_after_reject": 5120,
            "push_warming_bytes_after_recovery": 5120,
            "valid_body_len": 4096,
            "corrupt_body_len": 4096,
            "corrupt_response_body_len": 80,
            "recovery_body_len": 4096,
            "second_body_len": 4096,
        }
        for idx, pattern in enumerate(
            [
                ".crab/xorbs/{hash}",
                ".crab/shards/{hash}",
            ]
        )
    ]


def mutable_route_behavior_records() -> list[dict[str, Any]]:
    return [
        {
            "name": f"route-contract-mutable-{idx}",
            "pattern": pattern,
            "key": f"route-contract/mutable/{idx}",
            "status": 400,
            "cache_status": "",
            "origin_gets_before": 0,
            "origin_gets_after": 0,
            "body_len": 19,
        }
        for idx, pattern in enumerate(
            [
                "{repo}/refs/heads/*",
                "{repo}/HEAD",
                "{repo}/locks/*",
                "{repo}/packs/pack-{id}.meta",
                "{repo}/manifests/*",
                "{repo}/pack-list",
                "{repo}/shard-list",
                ".crab/ref-registry",
                "{repo}/file_index_db/manifest/current",
                ".crab/chunk_index_db/manifest/current",
            ]
        )
    ]


def mutable_route_write_behavior_records() -> list[dict[str, Any]]:
    return [
        {
            "name": f"route-contract-mutable-write-{idx}",
            "pattern": pattern,
            "key": f"route-contract/mutable/{idx}",
            "status": 400,
            "cache_status": "",
            "origin_gets_before": 0,
            "origin_gets_after": 0,
            "origin_puts_before": 0,
            "origin_puts_after": 0,
            "total_origin_gets_before": 11,
            "total_origin_gets_after": 11,
            "total_origin_puts_before": 13,
            "total_origin_puts_after": 13,
            "total_bytes_before": 4096,
            "total_bytes_after": 4096,
            "push_warming_writes_before": 1,
            "push_warming_writes_after": 1,
            "push_warming_bytes_before": 1024,
            "push_warming_bytes_after": 1024,
            "request_body_len": 257,
            "response_body_len": 36,
        }
        for idx, pattern in enumerate(
            [
                "{repo}/refs/heads/*",
                "{repo}/HEAD",
                "{repo}/locks/*",
                "{repo}/packs/pack-{id}.meta",
                "{repo}/manifests/*",
                "{repo}/pack-list",
                "{repo}/shard-list",
                ".crab/ref-registry",
                "{repo}/file_index_db/manifest/current",
                ".crab/chunk_index_db/manifest/current",
            ]
        )
    ]


def hydrate_record(name: str) -> dict[str, Any]:
    return {
        "name": name,
        "origin_gets_before": 7,
        "origin_gets_after": 7,
        "origin_get_key_delta": {},
        "cache_hits_delta": 4,
        "origin_fetches_delta": 0,
        "origin_avoided_reads_delta": 4,
        "mutable_read_rejections_delta": 0,
        "mutable_write_rejections_delta": 0,
        "hydrated_sha256": SHA256,
    }


def restart_persistence_records() -> list[dict[str, Any]]:
    return [
        {
            "name": "cache-server-restart-persistence",
            "direct_key": "e2e-cache-service/test/direct/restart/packs/abc.pack",
            "old_cache_service_url": "http://127.0.0.1:50001",
            "new_cache_service_url": "http://127.0.0.1:50002",
            "cache_root": "/tmp/crab-cache-service-smoke/server-cache",
            "direct_status": 200,
            "direct_cache_status": "HIT",
            "range_status": 206,
            "range_cache_status": "HIT",
            "direct_origin_gets_before": 1,
            "direct_origin_gets_after_direct": 1,
            "direct_origin_gets_after_range": 1,
            "total_origin_gets_before_direct": 10,
            "total_origin_gets_after_direct": 10,
            "total_origin_gets_after_range": 10,
            "direct_body_len": 4096,
            "range_body_len": 8,
            "cli_origin_gets_before": 10,
            "cli_origin_gets_after": 10,
            "cli_origin_get_key_delta": {},
            "cli_cache_hits_delta": 4,
            "cli_origin_fetches_delta": 0,
            "cli_origin_avoided_reads_delta": 4,
            "cli_mutable_read_rejections_delta": 0,
            "cli_mutable_write_rejections_delta": 0,
            "cli_hydrated_sha256": SHA256,
        }
    ]


def cache_integrity_repair_records() -> list[dict[str, Any]]:
    records = []
    for idx, (pattern, object_type) in enumerate(
        ((".crab/xorbs/{hash}", "xorb"), (".crab/shards/{hash}", "shard"))
    ):
        records.append(
            {
                "name": f"persisted-cache-integrity-repair-{object_type}",
                "pattern": pattern,
                "key": f".crab/{object_type}s/{idx + 1:064x}",
                "object_type": object_type,
                "cache_file": f"/tmp/crab-cache-service-smoke/server-cache/{object_type}s/00/{idx + 1:064x}",
                "old_cache_service_url": "http://127.0.0.1:50002",
                "new_cache_service_url": "http://127.0.0.1:50003",
                "corrupt_body_len": 4096,
                "valid_body_len": 4096,
                "repair_status": 200,
                "repair_cache_status": "MISS",
                "second_status": 200,
                "second_cache_status": "HIT",
                "origin_gets_before_repair": 1,
                "origin_gets_after_repair": 2,
                "origin_gets_after_second": 2,
                "total_origin_gets_before_repair": 10 + idx,
                "total_origin_gets_after_repair": 11 + idx,
                "total_origin_gets_after_second": 11 + idx,
                "total_bytes_before_repair": 8192,
                "total_bytes_after_repair": 8192,
                "total_bytes_after_second": 8192,
                "runtime_invalid_objects_evicted_before": idx,
                "runtime_invalid_objects_evicted_after_repair": idx + 1,
                "runtime_invalid_objects_evicted_after_second": idx + 1,
                "runtime_missing_files_repaired_before": 0,
                "runtime_missing_files_repaired_after_second": 0,
                "runtime_metadata_entries_recreated_before": 0,
                "runtime_metadata_entries_recreated_after_second": 0,
                "startup_integrity_repairs_after_restart": 0,
                "repair_body_len": 4096,
                "second_body_len": 4096,
            }
        )
    return records


def origin_outage_records() -> list[dict[str, Any]]:
    return [
        {
            "name": "origin-outage-cached-read-through",
            "hot_key": f"e2e-cache-service/{RUN_ID}/direct/origin-outage-hot/packs/{'1' * 64}.pack",
            "cold_key": f"e2e-cache-service/{RUN_ID}/direct/origin-outage-cold/packs/{'2' * 64}.pack",
            "health_status": 503,
            "live_status": 200,
            "warm_status": 200,
            "warm_cache_status": "MISS",
            "hot_status": 200,
            "hot_cache_status": "HIT",
            "range_status": 206,
            "range_cache_status": "HIT",
            "cold_status": 504,
            "cold_cache_status": "",
            "hot_origin_gets_before_outage": 1,
            "hot_origin_gets_after_hot": 1,
            "hot_origin_gets_after_range": 1,
            "cold_origin_gets_before_outage": 0,
            "cold_origin_gets_after_cold": 0,
            "total_origin_gets_before_outage": 12,
            "total_origin_gets_after_hot": 12,
            "total_origin_gets_after_range": 12,
            "total_origin_gets_after_cold": 12,
            "cache_hits_before_outage": 10,
            "cache_hits_after_outage": 12,
            "origin_fetches_before_outage": 8,
            "origin_fetches_after_outage": 8,
            "hot_body_len": 4096,
            "range_body_len": 24,
            "cold_body_len": 18,
        }
    ]


def report_payload() -> dict[str, Any]:
    return {
        "status": "passed",
        "run_id": RUN_ID,
        "bucket": "crab",
        "artifacts": {
            "report": "report.json",
            "cache_server_preflight_json": "cache-server-preflight.json",
            "cache_service_evidence_manifest": "cache-service-evidence-manifest.json",
            "cache_server_config": "cache-server.toml",
            "transparent_cache_server_config": "transparent-cache-server.toml",
            "cache_server_policy": "policy.yaml",
            "onboarding_check_json": "onboarding-check.json",
            "onboarding_probe_json": "onboarding-probe.json",
            "onboarding_client_probe_json": "onboarding-client-probe.json",
            "onboarding_client_config": "onboarding-client-config.toml",
            "onboarding_client_env": "onboarding-client.env",
            "onboarding_readme": "onboarding-README.md",
            "rustfs_smoke_script": "rustfs-smoke-script.py",
            "smoke_report_verifier": "smoke-report-verifier.py",
        },
        "checks": embedded_checks(),
        "env": {
            "AWS_ACCESS_KEY_ID": "<redacted>",
            "AWS_SECRET_ACCESS_KEY": "<redacted>",
            "AWS_SESSION_TOKEN": "<redacted>",
            "CRAB_CACHE_PSK": "<redacted>",
        },
        "auth_controls": auth_control_records(),
        "transparent_mutable_controls": transparent_mutable_records(),
        "capabilities": [
            {
                "name": "cache-service-capabilities",
                "status": 200,
                "schema": "crab-cache-service.capabilities.v1",
                "route_schema": "crab-cache-service.routes.v1",
                "route_transport_prefix": "/v1/",
                "immutable_route_patterns": [
                    ".crab/xorbs/{hash}",
                    ".crab/shards/{hash}",
                    "{repo}/packs/pack-{id}.pack",
                    "{repo}/packs/pack-{id}.idx",
                    "{repo}/file_index_db/compacted/*.sst",
                    "{repo}/file_index_db/manifest/*.manifest",
                    "{repo}/file_index_db/wal/*.sst",
                    "{repo}/file_index_db/compactions/*.compactions",
                    ".crab/chunk_index_db/compacted/*.sst",
                    ".crab/chunk_index_db/manifest/*.manifest",
                    ".crab/chunk_index_db/wal/*.sst",
                    ".crab/chunk_index_db/compactions/*.compactions",
                ],
                "mutable_route_patterns": [
                    "{repo}/refs/heads/*",
                    "{repo}/HEAD",
                    "{repo}/locks/*",
                    "{repo}/packs/pack-{id}.meta",
                    "{repo}/manifests/*",
                    "{repo}/pack-list",
                    "{repo}/shard-list",
                    ".crab/ref-registry",
                    "{repo}/file_index_db/manifest/current",
                    ".crab/chunk_index_db/manifest/current",
                ],
                "max_cache_bytes": 16777216,
                "admin_max_cache_bytes": 16777216,
                "max_object_bytes": 1048576,
                "admin_max_object_bytes": 1048576,
            },
        ],
        "request_limits": [
            {
                "name": "oversized-push-warming-rejected-before-body",
                "status": 413,
                "max_object_bytes": 1048576,
                "declared_content_length": 1048577,
                "body_bytes_sent": 0,
                "origin_gets_before": 0,
                "origin_gets_after": 0,
                "origin_puts_before": 0,
                "origin_puts_after": 0,
                "total_origin_gets_before": 2,
                "total_origin_gets_after": 2,
                "total_origin_puts_before": 3,
                "total_origin_puts_after": 3,
                "total_bytes_before": 4096,
                "total_bytes_after": 4096,
                "xorb_count_before": 1,
                "xorb_count_after": 1,
                "push_warming_writes_before": 1,
                "push_warming_writes_after": 1,
                "push_warming_bytes_before": 1024,
                "push_warming_bytes_after": 1024,
            },
        ],
        "reads": read_records(),
        "immutable_route_behaviors": immutable_route_behavior_records(),
        "immutable_route_write_behaviors": immutable_route_write_behavior_records(),
        "immutable_poisoning_controls": immutable_poisoning_control_records(),
        "mutable_route_behaviors": mutable_route_behavior_records(),
        "mutable_route_write_behaviors": mutable_route_write_behavior_records(),
        "cli_hydrates": [
            hydrate_record("cli-cold-hydrate"),
            hydrate_record("cli-warm-hydrate"),
            hydrate_record("restart-cli-hydrate"),
        ],
        "restart_persistence": restart_persistence_records(),
        "cache_integrity_repairs": cache_integrity_repair_records(),
        "origin_outages": origin_outage_records(),
        "cli_push_dedup": [
            {
                "name": "cli-dedup-push",
                "dedup_queries_delta": 1,
                "dedup_known_chunks_delta": 8,
                "dedup_unknown_chunks_delta": 0,
                "xorb_puts_delta": 0,
                "xorb_gets_delta": 0,
                "shard_gets_delta": 0,
                "metadata_gets_delta": 0,
                "cacheable_origin_gets_delta": 0,
                "cacheable_origin_get_key_delta": {},
                "origin_get_key_delta": {MANIFEST_KEY: 1},
                "origin_gets_delta": 1,
                "mutable_origin_get_key_delta": {MANIFEST_KEY: 1},
                "mutable_origin_gets_delta": 1,
                "mutable_read_rejections_delta": 0,
                "mutable_write_rejections_delta": 0,
            },
        ],
        "cache_pressure": [
            {
                "name": "cache-pressure",
                "total_bytes_after": 1024,
                "max_bytes": 2048,
                "expected_bytes_without_eviction": 4096,
                "evictions_before": 0,
                "evictions_after": 2,
                "hot_origin_gets_before": 1,
                "hot_origin_gets_after": 1,
            },
        ],
        "support_bundles": [
            {
                "name": "post-traffic",
                "schema": "cache-service.support-bundle",
                "health_ok": True,
                "health_status": 200,
                "auth_ok": True,
                "auth_status": 200,
                "auth_endpoint": "/v1/capabilities",
                "capabilities_ok": True,
                "capabilities_status": 200,
                "authz_ok": True,
                "authz_status": 200,
                "admin_stats_ok": True,
                "admin_stats_status": 200,
                "metrics_ok": True,
                "metrics_status": 200,
                "capabilities_schema": "crab-cache-service.capabilities.v1",
                "capabilities_max_cache_bytes": 16777216,
                "capabilities_max_object_bytes": 1048576,
                "authz_schema": "crab-cache-service.authz-check.v1",
                "authz_read": True,
                "authz_write": True,
                "authz_dedup": True,
                "authz_admin": True,
                "cache_hit_rate": 0.5,
                "origin_fallback_rate": 0.1,
                "integrity_repairs": 2,
                "push_warming_writes": 1,
                "evicted_objects": 1,
                "cache_max_bytes": 16777216.0,
                "cache_hit_total": 4.0,
                "origin_avoided_reads_total": 4.0,
                "origin_fetch_total": 2.0,
                "max_object_bytes": 1048576,
                "cache_max_object_bytes": 1048576.0,
            },
            {
                "name": "origin-outage",
                "schema": "cache-service.support-bundle",
                "health_ok": False,
                "health_status": 503,
                "auth_ok": True,
                "auth_status": 200,
                "auth_endpoint": "/v1/capabilities",
                "capabilities_ok": True,
                "capabilities_status": 200,
                "authz_ok": True,
                "authz_status": 200,
                "admin_stats_ok": True,
                "admin_stats_status": 200,
                "metrics_ok": True,
                "metrics_status": 200,
                "capabilities_schema": "crab-cache-service.capabilities.v1",
                "capabilities_max_cache_bytes": 16777216,
                "capabilities_max_object_bytes": 1048576,
                "authz_schema": "crab-cache-service.authz-check.v1",
                "authz_read": True,
                "authz_write": True,
                "authz_dedup": True,
                "authz_admin": True,
                "cache_hit_rate": 0.5,
                "origin_fallback_rate": 0.1,
                "integrity_repairs": 2,
                "push_warming_writes": 1,
                "evicted_objects": 1,
                "cache_max_bytes": 16777216.0,
                "cache_hit_total": 4.0,
                "origin_avoided_reads_total": 4.0,
                "origin_fetch_total": 2.0,
                "max_object_bytes": 1048576,
                "cache_max_object_bytes": 1048576.0,
            },
        ],
        "enterprise_onboarding": [
            {
                "name": "rendered-bundle",
                "bundle": "../private/enterprise-onboarding",
                "check_status": "ok",
                "probe_status": "warn",
                "server_config": "cache-server.toml",
                "policy": "policy.yaml",
                "client_config": "onboarding-client-config.toml",
                "client_env": "onboarding-client.env",
            },
        ],
    }


def write_json(path: Path, payload: dict[str, Any]) -> None:
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def sha256_file(path: Path) -> str:
    digest = sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def artifact_ref(report_dir: Path, path: Path) -> str:
    return os.path.relpath(path.resolve(), report_dir.resolve())


def file_evidence(report_dir: Path, path: Path) -> dict[str, Any]:
    path = path.resolve()
    return {
        "path": artifact_ref(report_dir, path),
        "sha256": sha256_file(path),
        "bytes": path.stat().st_size,
    }


def write_fixture(root: Path) -> tuple[Path, Path]:
    report_path = root / "report.json"
    preflight_path = root / "cache-server-preflight.json"
    manifest_path = root / "cache-service-evidence-manifest.json"
    config_path = root / "cache-server.toml"
    transparent_config_path = root / "transparent-cache-server.toml"
    policy_path = root / "policy.yaml"
    onboarding_check_path = root / "onboarding-check.json"
    onboarding_probe_path = root / "onboarding-probe.json"
    onboarding_client_probe_path = root / "onboarding-client-probe.json"
    onboarding_client_config_path = root / "onboarding-client-config.toml"
    onboarding_client_env_path = root / "onboarding-client.env"
    onboarding_readme_path = root / "onboarding-README.md"
    smoke_script_path = root / "rustfs-smoke-script.py"
    verifier_script_path = root / "smoke-report-verifier.py"
    smoke_script_path.write_bytes(SMOKE_SCRIPT.read_bytes())
    verifier_script_path.write_bytes(SCRIPT.read_bytes())
    config_path.write_text(REDACTED_CONFIG, encoding="utf-8")
    transparent_config_path.write_text(REDACTED_CONFIG, encoding="utf-8")
    policy_path.write_text(REDACTED_POLICY, encoding="utf-8")
    write_json(
        onboarding_check_path,
        {
            "status": "ok",
            "bundle_dir": "/tmp/cache-service/enterprise-onboarding",
            "checks": [{"name": "bundle file", "status": "ok", "detail": "README.md present"}],
        },
    )
    write_json(
        onboarding_probe_path,
        {
            "status": "warn",
            "bundle_check": {
                "status": "ok",
                "bundle_dir": "/tmp/cache-service/enterprise-onboarding",
                "checks": [{"name": "bundle file", "status": "ok", "detail": "README.md present"}],
            },
            "server_preflight": preflight_payload(),
        },
    )
    write_json(
        onboarding_client_probe_path,
        {
            "status": "ok",
            "bundle_check": {
                "status": "ok",
                "bundle_dir": "/tmp/cache-service/enterprise-onboarding",
                "checks": [{"name": "bundle file", "status": "ok", "detail": "README.md present"}],
            },
            "server_preflight": preflight_payload(),
            "client_probe": {
                "status": "ok",
                "repo_path": "e2e-cache-service/test-run/client-config",
                "service_url": "http://127.0.0.1:8443/",
                "checks": [
                    {
                        "name": "client probe cache roundtrip",
                        "status": "ok",
                        "detail": "write/read/range/cleanup ok",
                    }
                ],
            },
        },
    )
    onboarding_client_config_path.write_text(
        """# Generated by `crab-cache-server onboarding render`.

[cache]
service_url = "http://127.0.0.1:8443"
service_mode = "cache+dedup"
service_auth = "psk"
push_warming = true
""",
        encoding="utf-8",
    )
    onboarding_client_env_path.write_text(
        """export CRAB_CACHE_SERVICE_URL='http://127.0.0.1:8443'
export CRAB_CACHE_PSK='<secret-from-secret-manager>'
""",
        encoding="utf-8",
    )
    onboarding_readme_path.write_text(
        "crab-cache-server onboarding check --bundle-dir . --json > onboarding-check.json\n",
        encoding="utf-8",
    )
    write_json(preflight_path, preflight_payload())
    write_json(report_path, report_payload())
    write_json(
        manifest_path,
        {
            "schema": EVIDENCE_MANIFEST_SCHEMA,
            "generated_at": "2026-06-21T00:00:00+00:00",
            "run_id": RUN_ID,
            "artifacts": {
                "report": file_evidence(root, report_path),
                "cache_server_preflight_json": file_evidence(root, preflight_path),
                "cache_server_config": file_evidence(root, config_path),
                "transparent_cache_server_config": file_evidence(root, transparent_config_path),
                "cache_server_policy": file_evidence(root, policy_path),
                "onboarding_check_json": file_evidence(root, onboarding_check_path),
                "onboarding_probe_json": file_evidence(root, onboarding_probe_path),
                "onboarding_client_probe_json": file_evidence(root, onboarding_client_probe_path),
                "onboarding_client_config": file_evidence(root, onboarding_client_config_path),
                "onboarding_client_env": file_evidence(root, onboarding_client_env_path),
                "onboarding_readme": file_evidence(root, onboarding_readme_path),
                "rustfs_smoke_script": file_evidence(root, smoke_script_path),
                "smoke_report_verifier": file_evidence(root, verifier_script_path),
            },
            "runtime": {
                "crab_bin": "/tmp/crab",
                "crab_version": "crab 1.0.1",
                "cache_server_bin": "/tmp/crab-cache-server",
                "cache_server_version": "crab-cache-server 1.0.1",
                "rustfs_endpoint": "http://127.0.0.1:9000",
                "rustfs_bucket": "crab",
            },
            "parameters": {
                "object_kib": 128,
                "cli_file_kib": 512,
                "max_cache_bytes": 16777216,
                "dedup_scope": "all",
                "mutable_path_mode": "strict",
            },
        },
    )
    return report_path, preflight_path


def run_verifier(report_path: Path, output_path: Path | None = None) -> subprocess.CompletedProcess[str]:
    command = [sys.executable, str(SCRIPT), str(report_path)]
    if output_path is not None:
        command.extend(["--output", str(output_path)])
    return subprocess.run(
        command,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def run_audit(report_path: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SMOKE_SCRIPT), "--audit-report", str(report_path)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def refresh_manifest_record(report_path: Path, key: str, path: Path) -> None:
    manifest_path = report_path.with_name("cache-service-evidence-manifest.json")
    manifest = load_json(manifest_path)
    manifest["artifacts"][key] = file_evidence(report_path.parent, path)
    write_json(manifest_path, manifest)


def assert_success() -> None:
    with tempfile.TemporaryDirectory(prefix="cache-report-verifier-ok-") as temp:
        report_path, _ = write_fixture(Path(temp))
        output_path = Path(temp) / "verification.json"
        result = run_verifier(report_path, output_path)
        if result.returncode != 0:
            fail("valid fixture should pass", result)
        summary = load_json(output_path)
        if summary.get("status") != "passed" or summary.get("verified_checks", 0) <= 0:
            raise AssertionError(f"unexpected success summary: {summary}")
        audit = run_audit(report_path)
        if audit.returncode != 0:
            fail("valid fixture should pass smoke audit mode", audit)


def assert_failure(
    name: str,
    mutate: Callable[[Path, Path], None],
    expected_error: str,
) -> None:
    with tempfile.TemporaryDirectory(prefix=f"cache-report-verifier-{name}-") as temp:
        report_path, preflight_path = write_fixture(Path(temp))
        output_path = Path(temp) / "verification.json"
        mutate(report_path, preflight_path)
        result = run_verifier(report_path, output_path)
        if result.returncode == 0:
            fail(f"{name} should fail", result)
        combined = f"{result.stdout}\n{result.stderr}"
        if expected_error not in combined:
            fail(f"{name} failed with the wrong error; wanted {expected_error}", result)
        summary = load_json(output_path)
        if summary.get("status") != "failed" or expected_error not in summary.get("error", ""):
            raise AssertionError(f"{name} wrote unexpected failure summary: {summary}")


def fail(message: str, result: subprocess.CompletedProcess[str]) -> None:
    raise AssertionError(
        f"{message}\n"
        f"returncode={result.returncode}\n"
        f"stdout={result.stdout}\n"
        f"stderr={result.stderr}"
    )


def mutate_preflight_principal(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(preflight_path)
    payload["redaction_regression"] = "psk-client"
    write_json(preflight_path, payload)
    refresh_manifest_record(report_path, "cache_server_preflight_json", preflight_path)


def mutate_preflight_hash(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(preflight_path)
    payload["redaction_regression"] = DEFAULT_PSK_BLAKE3
    write_json(preflight_path, payload)
    refresh_manifest_record(report_path, "cache_server_preflight_json", preflight_path)


def mutate_missing_preflight(report_path: Path, preflight_path: Path) -> None:
    preflight_path.unlink()


def mutate_report_hash(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(report_path)
    payload["hash_regression"] = "report changed after manifest"
    write_json(report_path, payload)


def mutate_absolute_report_artifact_path(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(report_path)
    payload["artifacts"]["cache_server_preflight_json"] = str(preflight_path.resolve())
    write_json(report_path, payload)
    refresh_manifest_record(report_path, "report", report_path)


def mutate_manifest_hash(report_path: Path, preflight_path: Path) -> None:
    manifest_path = report_path.with_name("cache-service-evidence-manifest.json")
    manifest = load_json(manifest_path)
    manifest["artifacts"]["cache_server_preflight_json"]["sha256"] = "0" * 64
    write_json(manifest_path, manifest)


def mutate_retained_config_secret(report_path: Path, preflight_path: Path) -> None:
    config_path = report_path.with_name("cache-server.toml")
    config_path.write_text(
        REDACTED_CONFIG.replace('psk_hash = "<redacted>"', f'psk_hash = "{DEFAULT_PSK_BLAKE3}"'),
        encoding="utf-8",
    )
    refresh_manifest_record(report_path, "cache_server_config", config_path)


def mutate_retained_policy_principal(report_path: Path, preflight_path: Path) -> None:
    policy_path = report_path.with_name("policy.yaml")
    policy_path.write_text(
        REDACTED_POLICY.replace('principal: "<redacted>"', 'principal: "psk-client"'),
        encoding="utf-8",
    )
    refresh_manifest_record(report_path, "cache_server_policy", policy_path)


def mutate_cacheable_origin_get(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(report_path)
    dedup = payload["cli_push_dedup"][0]
    dedup["cacheable_origin_gets_delta"] = 1
    dedup["cacheable_origin_get_key_delta"] = {".crab/xorbs/regression": 1}
    write_json(report_path, payload)
    refresh_manifest_record(report_path, "report", report_path)


def mutate_uncategorized_origin_get(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(report_path)
    dedup = payload["cli_push_dedup"][0]
    dedup["origin_gets_delta"] = 2
    dedup["origin_get_key_delta"]["unexpected/object"] = 1
    write_json(report_path, payload)
    refresh_manifest_record(report_path, "report", report_path)


def mutate_warm_hydrate_origin_fetch(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(report_path)
    payload["cli_hydrates"][1]["origin_fetches_delta"] = 1
    write_json(report_path, payload)
    refresh_manifest_record(report_path, "report", report_path)


def mutate_mutable_route_status(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(report_path)
    payload["mutable_route_behaviors"][0]["status"] = 200
    write_json(report_path, payload)
    refresh_manifest_record(report_path, "report", report_path)


def mutate_origin_outage_hot_origin_get(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(report_path)
    payload["origin_outages"][0]["hot_origin_gets_after_hot"] = 2
    write_json(report_path, payload)
    refresh_manifest_record(report_path, "report", report_path)


def mutate_origin_outage_support_bundle_health_ok(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(report_path)
    outage_bundle = next(
        bundle for bundle in payload["support_bundles"] if bundle.get("name") == "origin-outage"
    )
    outage_bundle["health_ok"] = True
    outage_bundle["health_status"] = 200
    write_json(report_path, payload)
    refresh_manifest_record(report_path, "report", report_path)


def mutate_onboarding_check_failed(report_path: Path, preflight_path: Path) -> None:
    onboarding_path = report_path.with_name("onboarding-check.json")
    payload = load_json(onboarding_path)
    payload["status"] = "fail"
    write_json(onboarding_path, payload)
    refresh_manifest_record(report_path, "onboarding_check_json", onboarding_path)


def mutate_onboarding_probe_failed(report_path: Path, preflight_path: Path) -> None:
    onboarding_path = report_path.with_name("onboarding-probe.json")
    payload = load_json(onboarding_path)
    payload["status"] = "fail"
    write_json(onboarding_path, payload)
    refresh_manifest_record(report_path, "onboarding_probe_json", onboarding_path)


def mutate_onboarding_client_probe_failed(report_path: Path, preflight_path: Path) -> None:
    onboarding_path = report_path.with_name("onboarding-client-probe.json")
    payload = load_json(onboarding_path)
    payload["client_probe"]["status"] = "fail"
    write_json(onboarding_path, payload)
    refresh_manifest_record(report_path, "onboarding_client_probe_json", onboarding_path)


def mutate_report_secret(report_path: Path, preflight_path: Path) -> None:
    payload = load_json(report_path)
    payload["secret_regression"] = "cache-smoke-psk"
    write_json(report_path, payload)


def main() -> int:
    try:
        assert_success()
        assert_failure(
            "preflight-principal",
            mutate_preflight_principal,
            "cache-server-preflight-omits-policy-principal",
        )
        assert_failure(
            "preflight-hash",
            mutate_preflight_hash,
            "cache-server-preflight-omits-default-psk-hash",
        )
        assert_failure(
            "missing-preflight",
            mutate_missing_preflight,
            "evidence-manifest-cache_server_preflight_json-file-exists",
        )
        assert_failure(
            "report-hash",
            mutate_report_hash,
            "evidence-manifest-report-sha256",
        )
        assert_failure(
            "absolute-report-artifact-path",
            mutate_absolute_report_artifact_path,
            "artifact-cache_server_preflight_json-relative",
        )
        assert_failure(
            "manifest-hash",
            mutate_manifest_hash,
            "evidence-manifest-cache_server_preflight_json-sha256",
        )
        assert_failure(
            "retained-config-secret",
            mutate_retained_config_secret,
            "retained-cache_server_config-secret-free",
        )
        assert_failure(
            "retained-policy-principal",
            mutate_retained_policy_principal,
            "retained-cache_server_policy-secret-free",
        )
        assert_failure(
            "cacheable-origin-get",
            mutate_cacheable_origin_get,
            "cli-dedup-cacheable-origin-get-zero",
        )
        assert_failure(
            "uncategorized-origin-get",
            mutate_uncategorized_origin_get,
            "cli-dedup-only-manifest-cas-origin-read",
        )
        assert_failure(
            "warm-hydrate-origin-fetch",
            mutate_warm_hydrate_origin_fetch,
            "cli-warm-hydrate-origin-fetches-flat",
        )
        assert_failure(
            "mutable-route-status",
            mutate_mutable_route_status,
            "route-contract-mutable-read-repo-refs-heads-status",
        )
        assert_failure(
            "origin-outage-hot-origin-get",
            mutate_origin_outage_hot_origin_get,
            "origin-outage-origin-flat-for-hot-hits",
        )
        assert_failure(
            "origin-outage-support-bundle-health-ok",
            mutate_origin_outage_support_bundle_health_ok,
            "origin-outage-support-bundle-health-degraded",
        )
        assert_failure(
            "onboarding-check-failed",
            mutate_onboarding_check_failed,
            "onboarding-check-json-status-ok",
        )
        assert_failure(
            "onboarding-probe-failed",
            mutate_onboarding_probe_failed,
            "onboarding-probe-json-status-ok-or-warn",
        )
        assert_failure(
            "onboarding-client-probe-failed",
            mutate_onboarding_client_probe_failed,
            "onboarding-active-client-probe-status-ok",
        )
        assert_failure(
            "report-secret",
            mutate_report_secret,
            "report contains forbidden secret literal",
        )
    except AssertionError as exc:
        print(f"FAILED cache-service smoke report verifier self-test: {exc}", file=sys.stderr)
        return 1

    print("PASS cache-service smoke report verifier self-test")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
