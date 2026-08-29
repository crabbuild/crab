#!/usr/bin/env python3
"""Verify cache-service RustFS smoke report traffic evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path
from typing import Any


DEFAULT_PSK_BLAKE3 = "4fb898757c4c93662343bbbb25419f8c4f9c979352d40ff896578cabf620cf6e"
DEFAULT_FORBIDDEN_SECRETS = ("cache-smoke-psk", DEFAULT_PSK_BLAKE3)
EVIDENCE_MANIFEST_SCHEMA = "crab-cache-service.evidence-manifest.v1"
EXPECTED_ROUTE_SCHEMA = "crab-cache-service.routes.v1"
EXPECTED_IMMUTABLE_ROUTE_PATTERNS = [
    ".crab/xorbs/{first-two-hex}/{hash}",
    ".crab/shards/{first-two-hex}/{hash}",
    "{repo}/packs/pack-{id}.pack",
    "{repo}/packs/pack-{id}.idx",
    "{repo}/generated-packs/v1/artifacts/{first-two-hex}/{hash}.pack",
    "{repo}/generated-packs/v1/requests/{first-two-hex}/{hash}.json",
    "{repo}/file_index_db/compacted/*.sst",
    "{repo}/file_index_db/manifest/*.manifest",
    "{repo}/file_index_db/wal/*.sst",
    "{repo}/file_index_db/compactions/*.compactions",
    ".crab/chunk_index_db/compacted/*.sst",
    ".crab/chunk_index_db/manifest/*.manifest",
    ".crab/chunk_index_db/wal/*.sst",
    ".crab/chunk_index_db/compactions/*.compactions",
]
EXPECTED_IMMUTABLE_POISONING_PATTERNS = [
    ".crab/xorbs/{first-two-hex}/{hash}",
    ".crab/shards/{first-two-hex}/{hash}",
]
EXPECTED_MUTABLE_ROUTE_PATTERNS = [
    "{repo}/refs/heads/*",
    "{repo}/HEAD",
    "{repo}/locks/*",
    "{repo}/packs/pack-{id}.meta",
    "{repo}/manifests/*",
    "{repo}/pack-list",
    "{repo}/shard-list",
    ".crab/ref-registry/*",
    "{repo}/file_index_db/manifest/current",
    ".crab/chunk_index_db/manifest/current",
]
MUTABLE_ROUTE_PATTERN_IDS = {
    "{repo}/refs/heads/*": "repo-refs-heads",
    "{repo}/HEAD": "repo-head",
    "{repo}/locks/*": "repo-locks",
    "{repo}/packs/pack-{id}.meta": "repo-pack-meta",
    "{repo}/manifests/*": "repo-manifests",
    "{repo}/pack-list": "repo-pack-list",
    "{repo}/shard-list": "repo-shard-list",
    ".crab/ref-registry/*": "global-ref-registry",
    "{repo}/file_index_db/manifest/current": "repo-file-index-current",
    ".crab/chunk_index_db/manifest/current": "global-chunk-index-current",
}
SECRET_ENV_KEYS = {
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "CRAB_CACHE_PSK",
}


def artifact_reference(path: Path, report_dir: Path) -> str:
    return os.path.relpath(Path(path).resolve(), report_dir.resolve())


class VerifyError(RuntimeError):
    """Raised when report evidence does not satisfy the release gate."""


class Verifier:
    def __init__(self, report: dict[str, Any], report_path: Path) -> None:
        self.report = report
        self.report_path = report_path.resolve()
        self.passed: list[dict[str, Any]] = []

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        if not ok:
            suffix = f": {json.dumps(detail, sort_keys=True)}" if detail else ""
            raise VerifyError(f"{name}{suffix}")
        self.passed.append({"name": name, "detail": detail or {}})

    def record(self, field: str, name: str) -> dict[str, Any]:
        records = self.report.get(field)
        self.check(f"{field}-is-list", isinstance(records, list), {"type": type(records).__name__})
        for record in records:
            if isinstance(record, dict) and record.get("name") == name:
                return record
        raise VerifyError(f"{field} missing record {name!r}")

    def records_by_pattern(self, field: str) -> dict[str, dict[str, Any]]:
        records = self.report.get(field)
        self.check(f"{field}-is-list", isinstance(records, list), {"type": type(records).__name__})
        if not isinstance(records, list):
            return {}
        by_pattern: dict[str, dict[str, Any]] = {}
        for record in records:
            if isinstance(record, dict) and isinstance(record.get("pattern"), str):
                by_pattern[record["pattern"]] = record
        return by_pattern

    def embedded_check(self, name: str) -> dict[str, Any]:
        checks = self.report.get("checks")
        self.check("embedded-checks-is-list", isinstance(checks, list), {"type": type(checks).__name__})
        for check in checks:
            if isinstance(check, dict) and check.get("name") == name:
                return check
        raise VerifyError(f"embedded checks missing {name!r}")

    def artifact_path(self, key: str) -> Path:
        artifacts = self.report.get("artifacts")
        self.check("artifacts-is-object", isinstance(artifacts, dict), {
            "type": type(artifacts).__name__,
        })
        value = artifacts.get(key)
        self.check(f"artifact-{key}-present", isinstance(value, str) and bool(value), {
            key: value,
        })
        path = Path(value)
        self.check(f"artifact-{key}-relative", not path.is_absolute(), {
            key: value,
        })
        path = self.report_path.parent / path
        return path.resolve()

    @staticmethod
    def int_value(record: dict[str, Any], key: str) -> int:
        value = record.get(key)
        if isinstance(value, bool) or not isinstance(value, int):
            raise VerifyError(f"{record.get('name', '<record>')}.{key} is not an integer")
        return value

    @staticmethod
    def float_value(record: dict[str, Any], key: str) -> float:
        value = record.get(key)
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise VerifyError(f"{record.get('name', '<record>')}.{key} is not numeric")
        return float(value)

    def verify(self) -> dict[str, Any]:
        self.verify_report_status()
        self.verify_evidence_manifest()
        self.verify_embedded_checks()
        self.verify_cache_server_preflight()
        self.verify_enterprise_onboarding()
        self.verify_doctor_cache_service_diagnostics()
        self.verify_doctor_active_probe()
        self.verify_secret_redaction()
        self.verify_auth_controls()
        self.verify_transparent_mutable_auth_controls()
        self.verify_capabilities()
        self.verify_route_contract_behavior()
        self.verify_request_limits()
        self.verify_direct_read_traffic()
        self.verify_cli_hydrate_traffic()
        self.verify_restart_persistence()
        self.verify_cache_integrity_repairs()
        self.verify_cli_dedup_traffic()
        self.verify_cache_pressure()
        self.verify_support_bundle_summary()
        self.verify_origin_outage()
        self.verify_origin_outage_support_bundle()
        return {
            "status": "passed",
            "run_id": self.report.get("run_id"),
            "verified_checks": len(self.passed),
            "checks": self.passed,
        }

    def verify_report_status(self) -> None:
        self.check("report-status-passed", self.report.get("status") == "passed", {
            "status": self.report.get("status"),
        })
        self.check("report-run-id-present", bool(self.report.get("run_id")), {
            "run_id": self.report.get("run_id"),
        })

    def verify_evidence_manifest(self) -> None:
        manifest_path = self.artifact_path("cache_service_evidence_manifest")
        self.check("evidence-manifest-artifact-exists", manifest_path.is_file(), {
            "path": str(manifest_path),
        })
        try:
            manifest = load_report(manifest_path)
        except VerifyError as exc:
            raise VerifyError(f"invalid evidence manifest: {exc}") from exc

        artifacts = manifest.get("artifacts")
        runtime = manifest.get("runtime")
        parameters = manifest.get("parameters")
        self.check("evidence-manifest-schema", manifest.get("schema") == EVIDENCE_MANIFEST_SCHEMA, {
            "schema": manifest.get("schema"),
        })
        self.check("evidence-manifest-run-id", manifest.get("run_id") == self.report.get("run_id"), {
            "manifest": manifest.get("run_id"),
            "report": self.report.get("run_id"),
        })
        self.check("evidence-manifest-artifacts-object", isinstance(artifacts, dict), {
            "type": type(artifacts).__name__,
        })

        artifact_paths = {
            "report": self.report_path,
            "cache_server_preflight_json": self.artifact_path("cache_server_preflight_json"),
            "rustfs_smoke_script": self.artifact_path("rustfs_smoke_script"),
            "smoke_report_verifier": self.artifact_path("smoke_report_verifier"),
        }
        artifacts_report = self.report.get("artifacts")
        for key in (
            "cache_server_config",
            "transparent_cache_server_config",
            "cache_server_policy",
            "onboarding_check_json",
            "onboarding_probe_json",
            "onboarding_client_probe_json",
            "onboarding_client_config",
            "onboarding_client_env",
            "onboarding_readme",
        ):
            if isinstance(artifacts_report, dict) and key in artifacts_report:
                artifact_paths[key] = self.artifact_path(key)
        for key, path in artifact_paths.items():
            self.verify_evidence_file_record(artifacts, key, path)

        self.check("evidence-manifest-runtime-object", isinstance(runtime, dict), {
            "type": type(runtime).__name__,
        })
        self.check(
            "evidence-manifest-crab-version-recorded",
            str(runtime.get("crab_version", "")).startswith("crab "),
            {"runtime": runtime},
        )
        self.check(
            "evidence-manifest-cache-server-version-recorded",
            str(runtime.get("cache_server_version", "")).startswith("crab-cache-server "),
            {"runtime": runtime},
        )
        self.check("evidence-manifest-bucket-matches-report", runtime.get("rustfs_bucket") == self.report.get("bucket"), {
            "runtime_bucket": runtime.get("rustfs_bucket"),
            "report_bucket": self.report.get("bucket"),
        })

        self.check("evidence-manifest-parameters-object", isinstance(parameters, dict), {
            "type": type(parameters).__name__,
        })
        self.check("evidence-manifest-dedup-scope", parameters.get("dedup_scope") == "all", {
            "parameters": parameters,
        })
        self.check("evidence-manifest-strict-mutable-path-mode", parameters.get("mutable_path_mode") == "strict", {
            "parameters": parameters,
        })
        self.verify_retained_config_artifacts()

    def verify_retained_config_artifacts(self) -> None:
        forbidden_literals = {
            "default-psk": "cache-smoke-psk",
            "default-psk-hash": DEFAULT_PSK_BLAKE3,
            "policy-principal": "psk-client",
        }
        for key in ("cache_server_config", "transparent_cache_server_config", "cache_server_policy"):
            path = self.artifact_path(key)
            self.check(f"retained-{key}-artifact-exists", path.is_file(), {
                "path": str(path),
            })
            text = path.read_text(encoding="utf-8")
            leaked = [
                label
                for label, literal in forbidden_literals.items()
                if literal in text
            ]
            self.check(f"retained-{key}-secret-free", not leaked, {
                "path": str(path),
                "leaked": leaked,
            })

    def verify_evidence_file_record(self, artifacts: dict[str, Any], key: str, path: Path) -> None:
        record = artifacts.get(key)
        self.check(f"evidence-manifest-{key}-record", isinstance(record, dict), {
            "record": record,
        })
        self.check(f"evidence-manifest-{key}-file-exists", path.is_file(), {
            "path": str(path),
        })
        expected_path = artifact_reference(path, self.report_path.parent)
        self.check(f"evidence-manifest-{key}-path", record.get("path") == expected_path, {
            "expected": expected_path,
            "actual": record.get("path"),
        })
        self.check(f"evidence-manifest-{key}-sha256", record.get("sha256") == sha256_file(path), {
            "path": str(path),
            "expected": sha256_file(path),
            "actual": record.get("sha256"),
        })
        self.check(f"evidence-manifest-{key}-bytes", record.get("bytes") == path.stat().st_size, {
            "path": str(path),
            "expected": path.stat().st_size,
            "actual": record.get("bytes"),
        })

    def verify_embedded_checks(self) -> None:
        checks = self.report.get("checks")
        self.check("embedded-checks-present", isinstance(checks, list) and bool(checks), {
            "count": len(checks) if isinstance(checks, list) else None,
        })
        failed = [
            check.get("name")
            for check in checks
            if not isinstance(check, dict) or check.get("ok") is not True
        ]
        self.check("embedded-checks-all-passed", not failed, {"failed": failed[:20]})

    def verify_cache_server_preflight(self) -> None:
        for name in (
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
        ):
            check = self.embedded_check(name)
            self.check(name, check.get("ok") is True, {"detail": check.get("detail")})

        path = self.artifact_path("cache_server_preflight_json")
        self.check("cache-server-preflight-artifact-exists", path.is_file(), {
            "path": str(path),
        })
        try:
            text = path.read_text(encoding="utf-8")
            payload = json.loads(text)
        except OSError as exc:
            raise VerifyError(f"cannot read cache server preflight artifact {path}: {exc}") from exc
        except json.JSONDecodeError as exc:
            raise VerifyError(f"invalid cache server preflight JSON {path}: {exc}") from exc
        self.check("cache-server-preflight-json-object", isinstance(payload, dict), {
            "type": type(payload).__name__,
        })

        summary = payload.get("summary")
        checks = payload.get("checks")
        self.check("cache-server-preflight-summary-object", isinstance(summary, dict), {
            "type": type(summary).__name__,
        })
        self.check("cache-server-preflight-checks-list", isinstance(checks, list), {
            "type": type(checks).__name__,
        })
        preflight_checks = {
            str(check.get("name")): check
            for check in checks
            if isinstance(check, dict)
        }
        issue_codes = {
            str(check.get("code"))
            for check in checks
            if isinstance(check, dict) and check.get("code")
        }
        enterprise = preflight_checks.get("enterprise profile")
        policy_diagnostics = summary.get("policy_diagnostics")

        self.check("cache-server-preflight-status", payload.get("status") in ("ok", "warn"), {
            "status": payload.get("status"),
            "codes": sorted(issue_codes),
        })
        self.check("cache-server-preflight-policy-configured", summary.get("policy") == "configured", {
            "summary": summary,
        })
        self.check("cache-server-preflight-strict-mutable-paths", summary.get("mutable_path_mode") == "strict", {
            "summary": summary,
        })
        self.check(
            "cache-server-preflight-max-object-bytes",
            isinstance(summary.get("max_object_bytes"), int) and summary["max_object_bytes"] > 0,
            {"max_object_bytes": summary.get("max_object_bytes")},
        )
        self.check(
            "cache-server-preflight-enterprise-ok",
            enterprise is not None and enterprise.get("status") == "ok",
            {"enterprise": enterprise},
        )
        self.check(
            "cache-server-preflight-no-enterprise-codes",
            not any(code.startswith("enterprise_") for code in issue_codes),
            {"codes": sorted(issue_codes)},
        )
        self.check(
            "cache-server-preflight-policy-diagnostics",
            policy_diagnostics
            == {
                "rule_count": 1,
                "repo_pattern_count": 2,
                "actions": ["read", "write", "dedup", "admin"],
            },
            {"policy_diagnostics": policy_diagnostics},
        )
        forbidden_literals = {
            "default-psk": "cache-smoke-psk",
            "default-psk-hash": DEFAULT_PSK_BLAKE3,
            "policy-principal": "psk-client",
        }
        for label, literal in forbidden_literals.items():
            self.check(
                f"cache-server-preflight-omits-{label}",
                literal not in text,
                {"label": label},
            )

    def verify_enterprise_onboarding(self) -> None:
        record = self.record("enterprise_onboarding", "rendered-bundle")
        self.check(
            "enterprise-onboarding-report-check-status-ok",
            record.get("check_status") == "ok",
            {"record": record},
        )
        self.check(
            "enterprise-onboarding-report-probe-status-ok-or-warn",
            record.get("probe_status") in ("ok", "warn"),
            {"record": record},
        )

        check_path = self.artifact_path("onboarding_check_json")
        probe_path = self.artifact_path("onboarding_probe_json")
        client_probe_path = self.artifact_path("onboarding_client_probe_json")
        client_config_path = self.artifact_path("onboarding_client_config")
        client_env_path = self.artifact_path("onboarding_client_env")
        readme_path = self.artifact_path("onboarding_readme")
        for key, path in {
            "onboarding_check_json": check_path,
            "onboarding_probe_json": probe_path,
            "onboarding_client_probe_json": client_probe_path,
            "onboarding_client_config": client_config_path,
            "onboarding_client_env": client_env_path,
            "onboarding_readme": readme_path,
        }.items():
            self.check(f"{key}-artifact-exists", path.is_file(), {"path": str(path)})

        try:
            check_text = check_path.read_text(encoding="utf-8")
            check_payload = json.loads(check_text)
        except OSError as exc:
            raise VerifyError(f"cannot read onboarding check artifact {check_path}: {exc}") from exc
        except json.JSONDecodeError as exc:
            raise VerifyError(f"invalid onboarding check JSON {check_path}: {exc}") from exc
        self.check(
            "onboarding-check-json-status-ok",
            check_payload.get("status") == "ok",
            {"status": check_payload.get("status")},
        )

        try:
            probe_text = probe_path.read_text(encoding="utf-8")
            probe_payload = json.loads(probe_text)
        except OSError as exc:
            raise VerifyError(f"cannot read onboarding probe artifact {probe_path}: {exc}") from exc
        except json.JSONDecodeError as exc:
            raise VerifyError(f"invalid onboarding probe JSON {probe_path}: {exc}") from exc
        self.check(
            "onboarding-probe-json-status-ok-or-warn",
            probe_payload.get("status") in ("ok", "warn"),
            {"status": probe_payload.get("status")},
        )
        self.check(
            "onboarding-probe-bundle-check-ok",
            probe_payload.get("bundle_check", {}).get("status") == "ok",
            {"bundle_check": probe_payload.get("bundle_check")},
        )

        try:
            client_probe_text = client_probe_path.read_text(encoding="utf-8")
            client_probe_payload = json.loads(client_probe_text)
        except OSError as exc:
            raise VerifyError(f"cannot read onboarding active client probe artifact {client_probe_path}: {exc}") from exc
        except json.JSONDecodeError as exc:
            raise VerifyError(f"invalid onboarding active client probe JSON {client_probe_path}: {exc}") from exc
        client_probe = client_probe_payload.get("client_probe")
        self.check(
            "onboarding-active-client-probe-json-status-ok",
            client_probe_payload.get("status") == "ok",
            {"status": client_probe_payload.get("status")},
        )
        self.check(
            "onboarding-active-client-probe-status-ok",
            isinstance(client_probe, dict) and client_probe.get("status") == "ok",
            {"client_probe": client_probe},
        )

        client_config = client_config_path.read_text(encoding="utf-8")
        for needle in (
            'service_mode = "cache+dedup"',
            'service_auth = "psk"',
            "push_warming = true",
        ):
            self.check(
                f"onboarding-client-config-has-{needle.split()[0]}",
                needle in client_config,
                {"needle": needle},
            )

        client_env = client_env_path.read_text(encoding="utf-8")
        self.check(
            "onboarding-client-env-has-service-url",
            "CRAB_CACHE_SERVICE_URL" in client_env,
        )
        self.check(
            "onboarding-client-env-has-psk-var",
            "CRAB_CACHE_PSK" in client_env,
        )

        readme = readme_path.read_text(encoding="utf-8")
        self.check(
            "onboarding-readme-has-ci-check-command",
            "crab-cache-server onboarding check --bundle-dir . --json > onboarding-check.json"
            in readme,
        )

        forbidden_literals = {
            "default-psk": "cache-smoke-psk",
            "default-psk-hash": DEFAULT_PSK_BLAKE3,
            "policy-principal": "psk-client",
        }
        for artifact_name, text in {
            "onboarding_check_json": check_text,
            "onboarding_probe_json": probe_text,
            "onboarding_client_probe_json": client_probe_text,
            "onboarding_client_config": client_config,
            "onboarding_client_env": client_env,
        }.items():
            leaked = [
                label
                for label, literal in forbidden_literals.items()
                if literal in text
            ]
            self.check(
                f"{artifact_name}-secret-free",
                not leaked,
                {"leaked": leaked},
            )

    def verify_doctor_cache_service_diagnostics(self) -> None:
        caps = self.embedded_check("doctor-cache-service-caps-ok")
        self.check("doctor-cache-service-caps-ok", caps.get("ok") is True, {
            "detail": caps.get("detail"),
        })
        authz = self.embedded_check("doctor-cache-service-authz-ok")
        self.check("doctor-cache-service-authz-ok", authz.get("ok") is True, {
            "detail": authz.get("detail"),
        })

    def verify_doctor_active_probe(self) -> None:
        active = self.embedded_check("doctor-cache-service-active-ok")
        self.check("doctor-active-probe-ok", active.get("ok") is True, {
            "detail": active.get("detail"),
        })
        self.check(
            "doctor-active-probe-cleanup-observed",
            "cleanup ok" in json.dumps(active.get("detail", {}), sort_keys=True),
            {"detail": active.get("detail")},
        )
        redaction = self.embedded_check("doctor-cache-service-active-secret-redacted")
        self.check("doctor-active-probe-secret-redacted", redaction.get("ok") is True, {
            "detail": redaction.get("detail"),
        })

    def verify_secret_redaction(self) -> None:
        env = self.report.get("env")
        self.check("env-is-object", isinstance(env, dict), {"type": type(env).__name__})
        leaked_env = {
            key: env.get(key)
            for key in SECRET_ENV_KEYS
            if key in env and env.get(key) != "<redacted>"
        }
        self.check("secret-env-redacted", not leaked_env, {"leaked_keys": sorted(leaked_env)})

    def verify_auth_controls(self) -> None:
        missing = self.record("auth_controls", "auth-missing-psk-rejected")
        self.check("auth-missing-psk-status", missing.get("status") == 401, {
            "status": missing.get("status"),
        })
        self.check("auth-missing-psk-no-origin-get", self.int_value(missing, "origin_gets_after") == self.int_value(missing, "origin_gets_before"), {
            "before": missing.get("origin_gets_before"),
            "after": missing.get("origin_gets_after"),
        })
        self.check("auth-missing-psk-no-cache-status", missing.get("cache_status") == "", {
            "cache_status": missing.get("cache_status"),
        })

        wrong = self.record("auth_controls", "auth-wrong-psk-rejected")
        self.check("auth-wrong-psk-status", wrong.get("status") == 401, {
            "status": wrong.get("status"),
        })
        self.check("auth-wrong-psk-no-origin-get", self.int_value(wrong, "origin_gets_after") == self.int_value(wrong, "origin_gets_before"), {
            "before": wrong.get("origin_gets_before"),
            "after": wrong.get("origin_gets_after"),
        })
        self.check("auth-wrong-psk-no-cache-status", wrong.get("cache_status") == "", {
            "cache_status": wrong.get("cache_status"),
        })

        valid = self.record("auth_controls", "auth-valid-psk-accepted")
        self.check("auth-valid-psk-status", valid.get("status") == 200, {
            "status": valid.get("status"),
        })
        self.check("auth-valid-psk-cache-miss", valid.get("cache_status") == "MISS", {
            "cache_status": valid.get("cache_status"),
        })
        self.check("auth-valid-psk-origin-fetch", self.int_value(valid, "origin_gets_after") == self.int_value(valid, "origin_gets_before") + 1, {
            "before": valid.get("origin_gets_before"),
            "after": valid.get("origin_gets_after"),
        })
        self.check("auth-valid-psk-body", self.int_value(valid, "body_len") > 0, {
            "body_len": valid.get("body_len"),
        })

        denied = self.record("auth_controls", "auth-policy-denies-out-of-scope-read")
        self.check("auth-policy-denied-status", denied.get("status") == 403, {
            "status": denied.get("status"),
        })
        self.check("auth-policy-denied-no-origin-get", self.int_value(denied, "origin_gets_after") == self.int_value(denied, "origin_gets_before"), {
            "before": denied.get("origin_gets_before"),
            "after": denied.get("origin_gets_after"),
        })
        self.check("auth-policy-denied-no-cache-status", denied.get("cache_status") == "", {
            "cache_status": denied.get("cache_status"),
        })

    def verify_transparent_mutable_auth_controls(self) -> None:
        allowed = self.record("transparent_mutable_controls", "transparent-mutable-allowed-get")
        self.check("transparent-mutable-allowed-get-status", allowed.get("status") == 200, {
            "status": allowed.get("status"),
        })
        self.check(
            "transparent-mutable-allowed-get-origin-fetch",
            self.int_value(allowed, "origin_gets_after")
            == self.int_value(allowed, "origin_gets_before") + 1,
            {
                "before": allowed.get("origin_gets_before"),
                "after": allowed.get("origin_gets_after"),
            },
        )
        self.check(
            "transparent-mutable-allowed-get-no-origin-head",
            self.int_value(allowed, "origin_heads_after")
            == self.int_value(allowed, "origin_heads_before"),
            {
                "before": allowed.get("origin_heads_before"),
                "after": allowed.get("origin_heads_after"),
            },
        )
        self.check(
            "transparent-mutable-allowed-get-proxy-count",
            self.int_value(allowed, "mutable_proxy_reads_after")
            == self.int_value(allowed, "mutable_proxy_reads_before") + 1,
            {
                "before": allowed.get("mutable_proxy_reads_before"),
                "after": allowed.get("mutable_proxy_reads_after"),
            },
        )
        self.check("transparent-mutable-allowed-get-body", self.int_value(allowed, "body_len") > 0, {
            "body_len": allowed.get("body_len"),
        })

        denied_get = self.record("transparent_mutable_controls", "transparent-mutable-denied-get")
        self.check("transparent-mutable-denied-get-status", denied_get.get("status") == 403, {
            "status": denied_get.get("status"),
        })
        self.check(
            "transparent-mutable-denied-get-no-origin",
            self.int_value(denied_get, "origin_gets_after")
            == self.int_value(denied_get, "origin_gets_before")
            and self.int_value(denied_get, "origin_heads_after")
            == self.int_value(denied_get, "origin_heads_before"),
            {
                "gets_before": denied_get.get("origin_gets_before"),
                "gets_after": denied_get.get("origin_gets_after"),
                "heads_before": denied_get.get("origin_heads_before"),
                "heads_after": denied_get.get("origin_heads_after"),
            },
        )
        self.check(
            "transparent-mutable-denied-get-no-proxy",
            self.int_value(denied_get, "mutable_proxy_reads_after")
            == self.int_value(denied_get, "mutable_proxy_reads_before"),
            {
                "before": denied_get.get("mutable_proxy_reads_before"),
                "after": denied_get.get("mutable_proxy_reads_after"),
            },
        )

        denied_head = self.record("transparent_mutable_controls", "transparent-mutable-denied-head")
        self.check("transparent-mutable-denied-head-status", denied_head.get("status") == 403, {
            "status": denied_head.get("status"),
        })
        self.check(
            "transparent-mutable-denied-head-no-origin",
            self.int_value(denied_head, "origin_gets_after")
            == self.int_value(denied_head, "origin_gets_before")
            and self.int_value(denied_head, "origin_heads_after")
            == self.int_value(denied_head, "origin_heads_before"),
            {
                "gets_before": denied_head.get("origin_gets_before"),
                "gets_after": denied_head.get("origin_gets_after"),
                "heads_before": denied_head.get("origin_heads_before"),
                "heads_after": denied_head.get("origin_heads_after"),
            },
        )
        self.check(
            "transparent-mutable-denied-head-no-proxy",
            self.int_value(denied_head, "mutable_proxy_reads_after")
            == self.int_value(denied_head, "mutable_proxy_reads_before"),
            {
                "before": denied_head.get("mutable_proxy_reads_before"),
                "after": denied_head.get("mutable_proxy_reads_after"),
            },
        )

        ambiguous = self.record("transparent_mutable_controls", "transparent-mutable-ambiguous-get")
        self.check("transparent-mutable-ambiguous-get-status", ambiguous.get("status") == 400, {
            "status": ambiguous.get("status"),
        })
        self.check(
            "transparent-mutable-ambiguous-get-no-origin",
            self.int_value(ambiguous, "origin_gets_after")
            == self.int_value(ambiguous, "origin_gets_before")
            and self.int_value(ambiguous, "origin_heads_after")
            == self.int_value(ambiguous, "origin_heads_before"),
            {
                "gets_before": ambiguous.get("origin_gets_before"),
                "gets_after": ambiguous.get("origin_gets_after"),
                "heads_before": ambiguous.get("origin_heads_before"),
                "heads_after": ambiguous.get("origin_heads_after"),
            },
        )
        self.check(
            "transparent-mutable-ambiguous-get-no-proxy",
            self.int_value(ambiguous, "mutable_proxy_reads_after")
            == self.int_value(ambiguous, "mutable_proxy_reads_before"),
            {
                "before": ambiguous.get("mutable_proxy_reads_before"),
                "after": ambiguous.get("mutable_proxy_reads_after"),
            },
        )

    def verify_capabilities(self) -> None:
        record = self.record("capabilities", "cache-service-capabilities")
        self.check("capabilities-status", record.get("status") == 200, {
            "status": record.get("status"),
        })
        self.check(
            "capabilities-schema",
            record.get("schema") == "crab-cache-service.capabilities.v1",
            {"schema": record.get("schema")},
        )
        self.check(
            "capabilities-route-schema",
            record.get("route_schema") == EXPECTED_ROUTE_SCHEMA,
            {"route_schema": record.get("route_schema")},
        )
        self.check(
            "capabilities-route-transport-prefix",
            record.get("route_transport_prefix") == "/v1/",
            {"route_transport_prefix": record.get("route_transport_prefix")},
        )
        self.check(
            "capabilities-immutable-route-contract",
            record.get("immutable_route_patterns") == EXPECTED_IMMUTABLE_ROUTE_PATTERNS,
            {"immutable_route_patterns": record.get("immutable_route_patterns")},
        )
        self.check(
            "capabilities-mutable-route-contract",
            record.get("mutable_route_patterns") == EXPECTED_MUTABLE_ROUTE_PATTERNS,
            {"mutable_route_patterns": record.get("mutable_route_patterns")},
        )
        self.check(
            "capabilities-cache-limit-matches-admin",
            self.int_value(record, "max_cache_bytes")
            == self.int_value(record, "admin_max_cache_bytes"),
            {
                "max_cache_bytes": record.get("max_cache_bytes"),
                "admin_max_cache_bytes": record.get("admin_max_cache_bytes"),
            },
        )
        self.check(
            "capabilities-object-limit-matches-admin",
            self.int_value(record, "max_object_bytes")
            == self.int_value(record, "admin_max_object_bytes"),
            {
                "max_object_bytes": record.get("max_object_bytes"),
                "admin_max_object_bytes": record.get("admin_max_object_bytes"),
            },
        )
        self.check("capabilities-cache-limit-positive", self.int_value(record, "max_cache_bytes") > 0, {
            "max_cache_bytes": record.get("max_cache_bytes"),
        })
        self.check("capabilities-object-limit-positive", self.int_value(record, "max_object_bytes") > 0, {
            "max_object_bytes": record.get("max_object_bytes"),
        })

    def verify_route_contract_behavior(self) -> None:
        immutable_records = self.report.get("immutable_route_behaviors")
        immutable = self.records_by_pattern("immutable_route_behaviors")
        self.check(
            "route-contract-immutable-behavior-count",
            isinstance(immutable_records, list)
            and len(immutable_records) == len(EXPECTED_IMMUTABLE_ROUTE_PATTERNS)
            and len(immutable) == len(EXPECTED_IMMUTABLE_ROUTE_PATTERNS),
            {
                "record_count": len(immutable_records) if isinstance(immutable_records, list) else None,
                "unique_patterns": len(immutable),
            },
        )
        self.check(
            "route-contract-immutable-behavior-patterns",
            sorted(immutable) == sorted(EXPECTED_IMMUTABLE_ROUTE_PATTERNS),
            {"patterns": sorted(immutable)},
        )
        for pattern in EXPECTED_IMMUTABLE_ROUTE_PATTERNS:
            record = immutable[pattern]
            name = f"route-contract-immutable-{pattern}"
            first_status = self.int_value(record, "first_status")
            first_cache_status = record.get("first_cache_status")
            first_origin_delta = self.int_value(record, "origin_gets_after_first") - self.int_value(
                record, "origin_gets_before"
            )
            self.check(f"{name}-first-status", first_status == 200, {"record": record})
            self.check(
                f"{name}-first-cache-status",
                first_cache_status in ("MISS", "HIT"),
                {"record": record},
            )
            self.check(
                f"{name}-first-origin-delta",
                first_origin_delta == (1 if first_cache_status == "MISS" else 0),
                {"record": record, "first_origin_delta": first_origin_delta},
            )
            self.check(f"{name}-body-present", self.int_value(record, "body_len") > 3, {"record": record})
            self.check(f"{name}-second-status", self.int_value(record, "second_status") == 200, {"record": record})
            self.check(f"{name}-second-hit", record.get("second_cache_status") == "HIT", {"record": record})
            self.check(
                f"{name}-second-origin-flat",
                self.int_value(record, "origin_gets_after_second")
                == self.int_value(record, "origin_gets_after_first"),
                {"record": record},
            )
            self.check(f"{name}-range-status", self.int_value(record, "range_status") == 206, {"record": record})
            self.check(f"{name}-range-hit", record.get("range_cache_status") == "HIT", {"record": record})
            self.check(
                f"{name}-range-body-present",
                self.int_value(record, "range_body_len") > 0,
                {"record": record},
            )
            self.check(
                f"{name}-range-origin-flat",
                self.int_value(record, "origin_gets_after_range")
                == self.int_value(record, "origin_gets_after_second"),
                {"record": record},
            )

        immutable_writes_records = self.report.get("immutable_route_write_behaviors")
        immutable_writes = self.records_by_pattern("immutable_route_write_behaviors")
        self.check(
            "route-contract-immutable-write-behavior-count",
            isinstance(immutable_writes_records, list)
            and len(immutable_writes_records) == len(EXPECTED_IMMUTABLE_ROUTE_PATTERNS)
            and len(immutable_writes) == len(EXPECTED_IMMUTABLE_ROUTE_PATTERNS),
            {
                "record_count": len(immutable_writes_records) if isinstance(immutable_writes_records, list) else None,
                "unique_patterns": len(immutable_writes),
            },
        )
        self.check(
            "route-contract-immutable-write-behavior-patterns",
            sorted(immutable_writes) == sorted(EXPECTED_IMMUTABLE_ROUTE_PATTERNS),
            {"patterns": sorted(immutable_writes)},
        )
        for pattern in EXPECTED_IMMUTABLE_ROUTE_PATTERNS:
            record = immutable_writes.get(pattern)
            name = f"route-contract-immutable-write-{pattern}"
            self.check(f"{name}-record-present", record is not None, {"pattern": pattern})
            if record is None:
                continue
            body_len = self.int_value(record, "body_len")
            self.check(f"{name}-evict-status", self.int_value(record, "evict_status") == 200, {"record": record})
            self.check(f"{name}-put-status", self.int_value(record, "put_status") == 201, {"record": record})
            self.check(f"{name}-put-cache-status-empty", record.get("put_cache_status") == "", {"record": record})
            self.check(f"{name}-get-status", self.int_value(record, "get_status") == 200, {"record": record})
            self.check(f"{name}-get-hit", record.get("get_cache_status") == "HIT", {"record": record})
            self.check(f"{name}-head-status", self.int_value(record, "head_status") == 200, {"record": record})
            self.check(f"{name}-head-hit", record.get("head_cache_status") == "HIT", {"record": record})
            self.check(f"{name}-range-status", self.int_value(record, "range_status") == 206, {"record": record})
            self.check(f"{name}-range-hit", record.get("range_cache_status") == "HIT", {"record": record})
            self.check(
                f"{name}-origin-gets-flat",
                self.int_value(record, "origin_gets_after_put")
                == self.int_value(record, "origin_gets_before")
                and self.int_value(record, "origin_gets_after_get")
                == self.int_value(record, "origin_gets_before")
                and self.int_value(record, "origin_gets_after_head")
                == self.int_value(record, "origin_gets_before")
                and self.int_value(record, "origin_gets_after_range")
                == self.int_value(record, "origin_gets_before"),
                {"record": record},
            )
            self.check(
                f"{name}-origin-puts-flat",
                self.int_value(record, "origin_puts_after")
                == self.int_value(record, "origin_puts_before"),
                {"record": record},
            )
            self.check(
                f"{name}-total-origin-traffic-flat",
                self.int_value(record, "total_origin_gets_after")
                == self.int_value(record, "total_origin_gets_before")
                and self.int_value(record, "total_origin_puts_after")
                == self.int_value(record, "total_origin_puts_before"),
                {"record": record},
            )
            self.check(
                f"{name}-cache-bytes-increased",
                self.int_value(record, "total_bytes_after")
                == self.int_value(record, "total_bytes_before") + body_len,
                {"record": record},
            )
            self.check(
                f"{name}-push-warming-recorded",
                self.int_value(record, "push_warming_writes_after")
                == self.int_value(record, "push_warming_writes_before") + 1
                and self.int_value(record, "push_warming_bytes_after")
                == self.int_value(record, "push_warming_bytes_before") + body_len,
                {"record": record},
            )
            self.check(f"{name}-body-present", body_len > 0, {"record": record})
            self.check(
                f"{name}-get-body-len",
                self.int_value(record, "get_body_len") == body_len,
                {"record": record},
            )
            self.check(
                f"{name}-range-body-present",
                self.int_value(record, "range_body_len") > 0,
                {"record": record},
            )

        poisoning_records = self.report.get("immutable_poisoning_controls")
        poisoning = self.records_by_pattern("immutable_poisoning_controls")
        self.check(
            "route-contract-immutable-poisoning-count",
            isinstance(poisoning_records, list)
            and len(poisoning_records) == len(EXPECTED_IMMUTABLE_POISONING_PATTERNS)
            and len(poisoning) == len(EXPECTED_IMMUTABLE_POISONING_PATTERNS),
            {
                "record_count": len(poisoning_records) if isinstance(poisoning_records, list) else None,
                "unique_patterns": len(poisoning),
            },
        )
        self.check(
            "route-contract-immutable-poisoning-patterns",
            sorted(poisoning) == sorted(EXPECTED_IMMUTABLE_POISONING_PATTERNS),
            {"patterns": sorted(poisoning)},
        )
        for pattern in EXPECTED_IMMUTABLE_POISONING_PATTERNS:
            record = poisoning.get(pattern)
            name = f"route-contract-immutable-poison-{pattern}"
            self.check(f"{name}-record-present", record is not None, {"pattern": pattern})
            if record is None:
                continue
            valid_body_len = self.int_value(record, "valid_body_len")
            self.check(f"{name}-evict-status", self.int_value(record, "evict_status") == 200, {"record": record})
            self.check(f"{name}-corrupt-status", self.int_value(record, "corrupt_status") == 409, {"record": record})
            self.check(f"{name}-corrupt-cache-status-empty", record.get("corrupt_cache_status") == "", {"record": record})
            self.check(
                f"{name}-corrupt-origin-get-flat",
                self.int_value(record, "origin_gets_after_reject")
                == self.int_value(record, "origin_gets_before"),
                {"record": record},
            )
            self.check(
                f"{name}-corrupt-origin-put-flat",
                self.int_value(record, "origin_puts_after")
                == self.int_value(record, "origin_puts_before"),
                {"record": record},
            )
            self.check(
                f"{name}-corrupt-total-origin-flat",
                self.int_value(record, "total_origin_gets_after_reject")
                == self.int_value(record, "total_origin_gets_before")
                and self.int_value(record, "total_origin_puts_after")
                == self.int_value(record, "total_origin_puts_before"),
                {"record": record},
            )
            self.check(
                f"{name}-corrupt-cache-bytes-flat",
                self.int_value(record, "total_bytes_after_reject")
                == self.int_value(record, "total_bytes_before"),
                {"record": record},
            )
            self.check(
                f"{name}-corrupt-push-warming-flat",
                self.int_value(record, "push_warming_writes_after_reject")
                == self.int_value(record, "push_warming_writes_before")
                and self.int_value(record, "push_warming_bytes_after_reject")
                == self.int_value(record, "push_warming_bytes_before"),
                {"record": record},
            )
            self.check(f"{name}-recovery-status", self.int_value(record, "recovery_status") == 200, {"record": record})
            self.check(f"{name}-recovery-miss", record.get("recovery_cache_status") == "MISS", {"record": record})
            self.check(
                f"{name}-recovery-origin-get-once",
                self.int_value(record, "origin_gets_after_recovery")
                == self.int_value(record, "origin_gets_before") + 1
                and self.int_value(record, "total_origin_gets_after_recovery")
                == self.int_value(record, "total_origin_gets_before") + 1,
                {"record": record},
            )
            self.check(
                f"{name}-recovery-push-warming-flat",
                self.int_value(record, "push_warming_writes_after_recovery")
                == self.int_value(record, "push_warming_writes_before")
                and self.int_value(record, "push_warming_bytes_after_recovery")
                == self.int_value(record, "push_warming_bytes_before"),
                {"record": record},
            )
            self.check(f"{name}-second-status", self.int_value(record, "second_status") == 200, {"record": record})
            self.check(f"{name}-second-hit", record.get("second_cache_status") == "HIT", {"record": record})
            self.check(
                f"{name}-second-origin-flat",
                self.int_value(record, "origin_gets_after_second")
                == self.int_value(record, "origin_gets_after_recovery")
                and self.int_value(record, "total_origin_gets_after_second")
                == self.int_value(record, "total_origin_gets_after_recovery"),
                {"record": record},
            )
            self.check(
                f"{name}-valid-body-lengths",
                valid_body_len > 0
                and self.int_value(record, "corrupt_body_len") == valid_body_len
                and self.int_value(record, "recovery_body_len") == valid_body_len
                and self.int_value(record, "second_body_len") == valid_body_len,
                {"record": record},
            )
            self.check(
                f"{name}-cache-restored-with-valid-body",
                self.int_value(record, "total_bytes_after_recovery")
                == self.int_value(record, "total_bytes_before") + valid_body_len,
                {"record": record},
            )
            self.check(
                f"{name}-conflict-body-present",
                self.int_value(record, "corrupt_response_body_len") > 0,
                {"record": record},
            )

        mutable_records = self.report.get("mutable_route_behaviors")
        mutable = self.records_by_pattern("mutable_route_behaviors")
        self.check(
            "route-contract-mutable-behavior-count",
            isinstance(mutable_records, list)
            and len(mutable_records) == len(EXPECTED_MUTABLE_ROUTE_PATTERNS)
            and len(mutable) == len(EXPECTED_MUTABLE_ROUTE_PATTERNS),
            {
                "record_count": len(mutable_records) if isinstance(mutable_records, list) else None,
                "unique_patterns": len(mutable),
            },
        )
        self.check(
            "route-contract-mutable-behavior-patterns",
            sorted(mutable) == sorted(EXPECTED_MUTABLE_ROUTE_PATTERNS),
            {"patterns": sorted(mutable)},
        )
        for pattern in EXPECTED_MUTABLE_ROUTE_PATTERNS:
            record = mutable[pattern]
            route_id = MUTABLE_ROUTE_PATTERN_IDS[pattern]
            name = f"route-contract-mutable-read-{route_id}"
            detail = {"route_id": route_id, "pattern": pattern, "record": record}
            self.check(f"{name}-status", self.int_value(record, "status") == 400, detail)
            self.check(f"{name}-cache-status-empty", record.get("cache_status") == "", detail)
            self.check(
                f"{name}-origin-flat",
                self.int_value(record, "origin_gets_after")
                == self.int_value(record, "origin_gets_before"),
                detail,
            )

        mutable_writes_records = self.report.get("mutable_route_write_behaviors")
        mutable_writes = self.records_by_pattern("mutable_route_write_behaviors")
        self.check(
            "route-contract-mutable-write-behavior-count",
            isinstance(mutable_writes_records, list)
            and len(mutable_writes_records) == len(EXPECTED_MUTABLE_ROUTE_PATTERNS)
            and len(mutable_writes) == len(EXPECTED_MUTABLE_ROUTE_PATTERNS),
            {
                "record_count": len(mutable_writes_records) if isinstance(mutable_writes_records, list) else None,
                "unique_patterns": len(mutable_writes),
            },
        )
        self.check(
            "route-contract-mutable-write-behavior-patterns",
            sorted(mutable_writes) == sorted(EXPECTED_MUTABLE_ROUTE_PATTERNS),
            {"patterns": sorted(mutable_writes)},
        )
        for pattern in EXPECTED_MUTABLE_ROUTE_PATTERNS:
            record = mutable_writes[pattern]
            route_id = MUTABLE_ROUTE_PATTERN_IDS[pattern]
            name = f"route-contract-mutable-write-{route_id}"
            detail = {"route_id": route_id, "pattern": pattern, "record": record}
            self.check(f"{name}-status", self.int_value(record, "status") == 400, detail)
            self.check(f"{name}-cache-status-empty", record.get("cache_status") == "", detail)
            self.check(
                f"{name}-origin-gets-flat",
                self.int_value(record, "origin_gets_after")
                == self.int_value(record, "origin_gets_before"),
                detail,
            )
            self.check(
                f"{name}-origin-puts-flat",
                self.int_value(record, "origin_puts_after")
                == self.int_value(record, "origin_puts_before"),
                detail,
            )
            self.check(
                f"{name}-total-origin-traffic-flat",
                self.int_value(record, "total_origin_gets_after")
                == self.int_value(record, "total_origin_gets_before")
                and self.int_value(record, "total_origin_puts_after")
                == self.int_value(record, "total_origin_puts_before"),
                detail,
            )
            self.check(
                f"{name}-cache-bytes-flat",
                self.int_value(record, "total_bytes_after")
                == self.int_value(record, "total_bytes_before"),
                detail,
            )
            self.check(
                f"{name}-push-warming-flat",
                self.int_value(record, "push_warming_writes_after")
                == self.int_value(record, "push_warming_writes_before")
                and self.int_value(record, "push_warming_bytes_after")
                == self.int_value(record, "push_warming_bytes_before"),
                detail,
            )
            self.check(
                f"{name}-request-body-present",
                self.int_value(record, "request_body_len") > 0,
                detail,
            )

    def verify_request_limits(self) -> None:
        record = self.record("request_limits", "oversized-push-warming-rejected-before-body")
        capabilities = self.record("capabilities", "cache-service-capabilities")
        max_object_bytes = self.int_value(record, "max_object_bytes")
        declared_content_length = self.int_value(record, "declared_content_length")
        self.check("request-limit-status", record.get("status") == 413, {
            "status": record.get("status"),
        })
        self.check("request-limit-max-object-bytes-present", max_object_bytes > 0, {
            "max_object_bytes": record.get("max_object_bytes"),
        })
        self.check(
            "request-limit-max-object-bytes-from-capabilities",
            max_object_bytes == self.int_value(capabilities, "max_object_bytes"),
            {
                "request_limit": record.get("max_object_bytes"),
                "capabilities": capabilities.get("max_object_bytes"),
            },
        )
        self.check(
            "request-limit-declared-length-from-live-limit",
            declared_content_length == max_object_bytes + 1,
            {
                "max_object_bytes": record.get("max_object_bytes"),
                "declared_content_length": record.get("declared_content_length"),
            },
        )
        self.check("request-limit-no-body-sent", self.int_value(record, "body_bytes_sent") == 0, {
            "body_bytes_sent": record.get("body_bytes_sent"),
        })
        self.check(
            "request-limit-no-origin-get-for-key",
            self.int_value(record, "origin_gets_after")
            == self.int_value(record, "origin_gets_before"),
            {
                "origin_gets_before": record.get("origin_gets_before"),
                "origin_gets_after": record.get("origin_gets_after"),
            },
        )
        self.check(
            "request-limit-no-origin-put-for-key",
            self.int_value(record, "origin_puts_after")
            == self.int_value(record, "origin_puts_before"),
            {
                "origin_puts_before": record.get("origin_puts_before"),
                "origin_puts_after": record.get("origin_puts_after"),
            },
        )
        self.check(
            "request-limit-no-total-origin-gets",
            self.int_value(record, "total_origin_gets_after")
            == self.int_value(record, "total_origin_gets_before"),
            {
                "total_origin_gets_before": record.get("total_origin_gets_before"),
                "total_origin_gets_after": record.get("total_origin_gets_after"),
            },
        )
        self.check(
            "request-limit-no-total-origin-puts",
            self.int_value(record, "total_origin_puts_after")
            == self.int_value(record, "total_origin_puts_before"),
            {
                "total_origin_puts_before": record.get("total_origin_puts_before"),
                "total_origin_puts_after": record.get("total_origin_puts_after"),
            },
        )
        self.check(
            "request-limit-cache-bytes-unchanged",
            self.int_value(record, "total_bytes_after")
            == self.int_value(record, "total_bytes_before"),
            {
                "total_bytes_before": record.get("total_bytes_before"),
                "total_bytes_after": record.get("total_bytes_after"),
            },
        )
        self.check(
            "request-limit-xorb-count-unchanged",
            self.int_value(record, "xorb_count_after") == self.int_value(record, "xorb_count_before"),
            {
                "xorb_count_before": record.get("xorb_count_before"),
                "xorb_count_after": record.get("xorb_count_after"),
            },
        )
        self.check(
            "request-limit-push-warming-writes-unchanged",
            self.int_value(record, "push_warming_writes_after")
            == self.int_value(record, "push_warming_writes_before"),
            {
                "push_warming_writes_before": record.get("push_warming_writes_before"),
                "push_warming_writes_after": record.get("push_warming_writes_after"),
            },
        )
        self.check(
            "request-limit-push-warming-bytes-unchanged",
            self.int_value(record, "push_warming_bytes_after")
            == self.int_value(record, "push_warming_bytes_before"),
            {
                "push_warming_bytes_before": record.get("push_warming_bytes_before"),
                "push_warming_bytes_after": record.get("push_warming_bytes_after"),
            },
        )

    def verify_direct_read_traffic(self) -> None:
        first = self.record("reads", "full-first-miss")
        second = self.record("reads", "full-second-hit")
        warm_range = self.record("reads", "warm-range-hit")

        self.check("full-first-miss-status", first.get("status") == 200, {"status": first.get("status")})
        self.check("full-first-miss-cache-status", first.get("cache_status") == "MISS", {
            "cache_status": first.get("cache_status"),
        })
        self.check("full-first-miss-origin-fetch", self.int_value(first, "origin_gets_for_key") == 1, {
            "origin_gets_for_key": first.get("origin_gets_for_key"),
        })
        self.check("full-first-miss-body", self.int_value(first, "body_len") > 0, {
            "body_len": first.get("body_len"),
        })

        self.check("full-second-hit-status", second.get("status") == 200, {"status": second.get("status")})
        self.check("full-second-hit-cache-status", second.get("cache_status") == "HIT", {
            "cache_status": second.get("cache_status"),
        })
        self.check("full-second-hit-same-key", second.get("key") == first.get("key"), {
            "first": first.get("key"),
            "second": second.get("key"),
        })
        self.check("full-second-hit-body", second.get("body_len") == first.get("body_len"), {
            "first": first.get("body_len"),
            "second": second.get("body_len"),
        })
        self.check(
            "full-second-hit-no-extra-origin-get",
            self.int_value(second, "origin_gets_for_key") == self.int_value(first, "origin_gets_for_key"),
            {
                "first": first.get("origin_gets_for_key"),
                "second": second.get("origin_gets_for_key"),
            },
        )

        self.check("warm-range-hit-status", warm_range.get("status") == 206, {
            "status": warm_range.get("status"),
        })
        self.check("warm-range-hit-cache-status", warm_range.get("cache_status") == "HIT", {
            "cache_status": warm_range.get("cache_status"),
        })
        self.check("warm-range-hit-same-key", warm_range.get("key") == first.get("key"), {
            "first": first.get("key"),
            "range": warm_range.get("key"),
        })
        self.check(
            "warm-range-hit-no-extra-origin-get",
            self.int_value(warm_range, "origin_gets_for_key")
            == self.int_value(first, "origin_gets_for_key"),
            {
                "first": first.get("origin_gets_for_key"),
                "range": warm_range.get("origin_gets_for_key"),
            },
        )

        cold_first = self.record("reads", "cold-range-first-miss")
        cold_second = self.record("reads", "cold-range-second-hit")
        self.check("cold-range-first-miss-status", cold_first.get("status") == 206, {
            "status": cold_first.get("status"),
        })
        self.check("cold-range-first-miss-cache-status", cold_first.get("cache_status") == "MISS", {
            "cache_status": cold_first.get("cache_status"),
        })
        self.check("cold-range-first-miss-origin-fetch", self.int_value(cold_first, "origin_gets_for_key") == 1, {
            "origin_gets_for_key": cold_first.get("origin_gets_for_key"),
        })
        self.check("cold-range-second-hit-cache-status", cold_second.get("cache_status") == "HIT", {
            "cache_status": cold_second.get("cache_status"),
        })
        self.check("cold-range-second-hit-same-key", cold_second.get("key") == cold_first.get("key"), {
            "first": cold_first.get("key"),
            "second": cold_second.get("key"),
        })
        self.check(
            "cold-range-second-hit-no-extra-origin-get",
            self.int_value(cold_second, "origin_gets_for_key")
            == self.int_value(cold_first, "origin_gets_for_key"),
            {
                "first": cold_first.get("origin_gets_for_key"),
                "second": cold_second.get("origin_gets_for_key"),
            },
        )

    def verify_cli_hydrate_traffic(self) -> None:
        for name in ("cli-cold-hydrate", "cli-warm-hydrate", "restart-cli-hydrate"):
            record = self.record("cli_hydrates", name)
            origin_delta = self.int_value(record, "origin_gets_after") - self.int_value(
                record, "origin_gets_before"
            )
            key_delta = record.get("origin_get_key_delta")
            if not isinstance(key_delta, dict):
                key_delta = {}
            immutable_key_delta = {
                key: count
                for key, count in key_delta.items()
                if str(key).startswith(".crab/xorbs/") or str(key).startswith(".crab/shards/")
            }
            self.check(
                f"{name}-immutable-origin-get-delta-zero",
                not immutable_key_delta,
                {"origin_delta": origin_delta, "immutable_key_delta": immutable_key_delta},
            )
            self.check(f"{name}-cache-hits-observed", self.int_value(record, "cache_hits_delta") > 0, {
                "cache_hits_delta": record.get("cache_hits_delta"),
            })
            self.check(f"{name}-origin-fetches-flat", self.int_value(record, "origin_fetches_delta") == 0, {
                "origin_fetches_delta": record.get("origin_fetches_delta"),
            })
            self.check(
                f"{name}-cache-service-mutable-read-rejections-flat",
                self.int_value(record, "mutable_read_rejections_delta") == 0,
                {
                    "mutable_read_rejections_delta": record.get(
                        "mutable_read_rejections_delta"
                    ),
                },
            )
            self.check(
                f"{name}-cache-service-mutable-write-rejections-flat",
                self.int_value(record, "mutable_write_rejections_delta") == 0,
                {
                    "mutable_write_rejections_delta": record.get(
                        "mutable_write_rejections_delta"
                    ),
                },
            )
            self.check(
                f"{name}-origin-avoidance-observed",
                self.int_value(record, "origin_avoided_reads_delta") > 0,
                {"origin_avoided_reads_delta": record.get("origin_avoided_reads_delta")},
            )
            sha = record.get("hydrated_sha256")
            self.check(f"{name}-hydrated-sha256-present", isinstance(sha, str) and len(sha) == 64, {
                "hydrated_sha256": sha,
            })

    def verify_restart_persistence(self) -> None:
        records = self.report.get("restart_persistence")
        self.check("restart-persistence-is-list", isinstance(records, list), {
            "type": type(records).__name__,
        })
        record = self.record("restart_persistence", "cache-server-restart-persistence")
        self.check(
            "restart-persistence-cache-root-present",
            isinstance(record.get("cache_root"), str) and bool(record.get("cache_root")),
            {"record": record},
        )
        self.check(
            "restart-persistence-url-changed",
            isinstance(record.get("old_cache_service_url"), str)
            and isinstance(record.get("new_cache_service_url"), str)
            and record.get("old_cache_service_url") != record.get("new_cache_service_url"),
            {"record": record},
        )
        self.check("restart-persistence-direct-status", self.int_value(record, "direct_status") == 200, {
            "record": record,
        })
        self.check("restart-persistence-direct-hit", record.get("direct_cache_status") == "HIT", {
            "record": record,
        })
        self.check("restart-persistence-range-status", self.int_value(record, "range_status") == 206, {
            "record": record,
        })
        self.check("restart-persistence-range-hit", record.get("range_cache_status") == "HIT", {
            "record": record,
        })
        self.check(
            "restart-persistence-direct-origin-flat",
            self.int_value(record, "direct_origin_gets_after_direct")
            == self.int_value(record, "direct_origin_gets_before")
            and self.int_value(record, "direct_origin_gets_after_range")
            == self.int_value(record, "direct_origin_gets_before")
            and self.int_value(record, "total_origin_gets_after_direct")
            == self.int_value(record, "total_origin_gets_before_direct")
            and self.int_value(record, "total_origin_gets_after_range")
            == self.int_value(record, "total_origin_gets_before_direct"),
            {"record": record},
        )
        self.check(
            "restart-persistence-direct-bodies-present",
            self.int_value(record, "direct_body_len") > 0
            and self.int_value(record, "range_body_len") > 0,
            {"record": record},
        )
        cli_key_delta = record.get("cli_origin_get_key_delta")
        if not isinstance(cli_key_delta, dict):
            cli_key_delta = {}
        cli_immutable_key_delta = {
            key: count
            for key, count in cli_key_delta.items()
            if str(key).startswith(".crab/xorbs/") or str(key).startswith(".crab/shards/")
        }
        self.check(
            "restart-persistence-cli-origin-flat",
            not cli_immutable_key_delta,
            {
                "record": record,
                "immutable_key_delta": cli_immutable_key_delta,
            },
        )
        self.check(
            "restart-persistence-cli-cache-hit",
            self.int_value(record, "cli_cache_hits_delta") > 0
            and self.int_value(record, "cli_origin_fetches_delta") == 0
            and self.int_value(record, "cli_origin_avoided_reads_delta") > 0,
            {"record": record},
        )
        self.check(
            "restart-persistence-cli-mutable-rejections-flat",
            self.int_value(record, "cli_mutable_read_rejections_delta") == 0
            and self.int_value(record, "cli_mutable_write_rejections_delta") == 0,
            {"record": record},
        )
        sha = record.get("cli_hydrated_sha256")
        self.check(
            "restart-persistence-cli-sha256-present",
            isinstance(sha, str) and len(sha) == 64,
            {"cli_hydrated_sha256": sha},
        )

    def verify_cache_integrity_repairs(self) -> None:
        records = self.report.get("cache_integrity_repairs")
        by_pattern = self.records_by_pattern("cache_integrity_repairs")
        self.check(
            "cache-integrity-repair-count",
            isinstance(records, list)
            and len(records) == len(EXPECTED_IMMUTABLE_POISONING_PATTERNS)
            and len(by_pattern) == len(EXPECTED_IMMUTABLE_POISONING_PATTERNS),
            {
                "record_count": len(records) if isinstance(records, list) else None,
                "unique_patterns": len(by_pattern),
            },
        )
        self.check(
            "cache-integrity-repair-patterns",
            sorted(by_pattern) == sorted(EXPECTED_IMMUTABLE_POISONING_PATTERNS),
            {"patterns": sorted(by_pattern)},
        )
        for pattern in EXPECTED_IMMUTABLE_POISONING_PATTERNS:
            record = by_pattern[pattern]
            name = f"cache-integrity-repair-{pattern}"
            self.check(
                f"{name}-url-changed",
                isinstance(record.get("old_cache_service_url"), str)
                and isinstance(record.get("new_cache_service_url"), str)
                and record.get("old_cache_service_url") != record.get("new_cache_service_url"),
                {"record": record},
            )
            self.check(
                f"{name}-cache-file-recorded",
                isinstance(record.get("cache_file"), str) and bool(record.get("cache_file")),
                {"record": record},
            )
            self.check(f"{name}-repair-status", self.int_value(record, "repair_status") == 200, {"record": record})
            self.check(f"{name}-repair-miss", record.get("repair_cache_status") == "MISS", {"record": record})
            self.check(f"{name}-second-status", self.int_value(record, "second_status") == 200, {"record": record})
            self.check(f"{name}-second-hit", record.get("second_cache_status") == "HIT", {"record": record})
            self.check(
                f"{name}-body-lengths",
                self.int_value(record, "valid_body_len") > 0
                and self.int_value(record, "corrupt_body_len") == self.int_value(record, "valid_body_len")
                and self.int_value(record, "repair_body_len") == self.int_value(record, "valid_body_len")
                and self.int_value(record, "second_body_len") == self.int_value(record, "valid_body_len"),
                {"record": record},
            )
            self.check(
                f"{name}-origin-refetch-once",
                self.int_value(record, "origin_gets_after_repair")
                == self.int_value(record, "origin_gets_before_repair") + 1
                and self.int_value(record, "total_origin_gets_after_repair")
                == self.int_value(record, "total_origin_gets_before_repair") + 1,
                {"record": record},
            )
            self.check(
                f"{name}-second-origin-flat",
                self.int_value(record, "origin_gets_after_second")
                == self.int_value(record, "origin_gets_after_repair")
                and self.int_value(record, "total_origin_gets_after_second")
                == self.int_value(record, "total_origin_gets_after_repair"),
                {"record": record},
            )
            self.check(
                f"{name}-runtime-invalid-eviction",
                self.int_value(record, "runtime_invalid_objects_evicted_after_repair")
                == self.int_value(record, "runtime_invalid_objects_evicted_before") + 1
                and self.int_value(record, "runtime_invalid_objects_evicted_after_second")
                == self.int_value(record, "runtime_invalid_objects_evicted_after_repair"),
                {"record": record},
            )
            self.check(
                f"{name}-other-runtime-repairs-flat",
                self.int_value(record, "runtime_missing_files_repaired_after_second")
                == self.int_value(record, "runtime_missing_files_repaired_before")
                and self.int_value(record, "runtime_metadata_entries_recreated_after_second")
                == self.int_value(record, "runtime_metadata_entries_recreated_before"),
                {"record": record},
            )
            self.check(
                f"{name}-startup-clean",
                self.int_value(record, "startup_integrity_repairs_after_restart") == 0,
                {"record": record},
            )
            self.check(
                f"{name}-cache-bytes-restored",
                self.int_value(record, "total_bytes_after_repair")
                == self.int_value(record, "total_bytes_before_repair")
                and self.int_value(record, "total_bytes_after_second")
                == self.int_value(record, "total_bytes_after_repair"),
                {"record": record},
            )

    def verify_cli_dedup_traffic(self) -> None:
        record = self.record("cli_push_dedup", "cli-dedup-push")
        self.check("cli-dedup-queries-observed", self.int_value(record, "dedup_queries_delta") > 0, {
            "dedup_queries_delta": record.get("dedup_queries_delta"),
        })
        self.check("cli-dedup-known-chunks-observed", self.int_value(record, "dedup_known_chunks_delta") > 0, {
            "dedup_known_chunks_delta": record.get("dedup_known_chunks_delta"),
        })
        self.check(
            "cli-dedup-no-unknown-chunks",
            self.int_value(record, "dedup_unknown_chunks_delta") == 0,
            {"dedup_unknown_chunks_delta": record.get("dedup_unknown_chunks_delta")},
        )
        self.check("cli-dedup-skipped-xorb-put", self.int_value(record, "xorb_puts_delta") == 0, {
            "xorb_puts_delta": record.get("xorb_puts_delta"),
        })
        self.check(
            "cli-dedup-canonical-xorb-proof",
            self.int_value(record, "xorb_gets_delta") > 0,
            {"xorb_gets_delta": record.get("xorb_gets_delta")},
        )
        self.check(
            "cli-dedup-canonical-shard-proof",
            self.int_value(record, "shard_gets_delta") > 0,
            {"shard_gets_delta": record.get("shard_gets_delta")},
        )
        self.check(
            "cli-dedup-metadata-read",
            self.int_value(record, "metadata_gets_delta") > 0,
            {"metadata_gets_delta": record.get("metadata_gets_delta")},
        )
        cacheable_keys = record.get("cacheable_origin_get_key_delta", {})
        if not isinstance(cacheable_keys, dict):
            cacheable_keys = {}
        self.check(
            "cli-dedup-cacheable-origin-proof",
            self.int_value(record, "cacheable_origin_gets_delta") > 0
            and any(str(key).startswith(".crab/xorbs/") for key in cacheable_keys)
            and any(str(key).startswith(".crab/shards/") for key in cacheable_keys),
            {
                "cacheable_origin_gets_delta": record.get("cacheable_origin_gets_delta"),
                "cacheable_origin_get_key_delta": record.get("cacheable_origin_get_key_delta"),
                "origin_get_key_delta": record.get("origin_get_key_delta"),
            },
        )
        self.check(
            "cli-dedup-mutable-read-rejections-flat",
            self.int_value(record, "mutable_read_rejections_delta") == 0,
            {
                "mutable_read_rejections_delta": record.get(
                    "mutable_read_rejections_delta"
                ),
            },
        )
        self.check(
            "cli-dedup-mutable-write-rejections-flat",
            self.int_value(record, "mutable_write_rejections_delta") == 0,
            {
                "mutable_write_rejections_delta": record.get(
                    "mutable_write_rejections_delta"
                ),
            },
        )

        run_id = self.report.get("run_id")
        expected_manifest = f"e2e-cache-service/{run_id}/cli-dedup/manifest"
        mutable_keys = record.get("mutable_origin_get_key_delta", {})
        if not isinstance(mutable_keys, dict):
            mutable_keys = {}
        self.check(
            "cli-dedup-manifest-cas-origin-read",
            int(mutable_keys.get(expected_manifest, 0)) > 0
            and self.int_value(record, "mutable_origin_gets_delta") > 0,
            {
                "expected_key": expected_manifest,
                "actual": record.get("origin_get_key_delta"),
                "mutable_actual": mutable_keys,
                "origin_gets_delta": record.get("origin_gets_delta"),
                "mutable_origin_gets_delta": record.get("mutable_origin_gets_delta"),
            },
        )

    def verify_cache_pressure(self) -> None:
        record = self.record("cache_pressure", "cache-pressure")
        self.check("cache-pressure-stayed-within-budget", self.int_value(record, "total_bytes_after") <= self.int_value(record, "max_bytes"), {
            "total_bytes_after": record.get("total_bytes_after"),
            "max_bytes": record.get("max_bytes"),
        })
        self.check(
            "cache-pressure-evicted-cold-objects",
            self.int_value(record, "total_bytes_after")
            < self.int_value(record, "expected_bytes_without_eviction"),
            {
                "total_bytes_after": record.get("total_bytes_after"),
                "expected_bytes_without_eviction": record.get("expected_bytes_without_eviction"),
            },
        )
        self.check(
            "cache-pressure-evictions-increased",
            self.int_value(record, "evictions_after") > self.int_value(record, "evictions_before"),
            {
                "evictions_before": record.get("evictions_before"),
                "evictions_after": record.get("evictions_after"),
            },
        )
        self.check(
            "cache-pressure-hot-object-stayed-warm",
            self.int_value(record, "hot_origin_gets_after")
            == self.int_value(record, "hot_origin_gets_before"),
            {
                "hot_origin_gets_before": record.get("hot_origin_gets_before"),
                "hot_origin_gets_after": record.get("hot_origin_gets_after"),
            },
        )

    def verify_support_bundle_summary(self) -> None:
        record = self.record("support_bundles", "post-traffic")
        capabilities = self.record("capabilities", "cache-service-capabilities")
        request_limit = self.record("request_limits", "oversized-push-warming-rejected-before-body")
        self.check("support-bundle-schema", record.get("schema") == "cache-service.support-bundle", {
            "schema": record.get("schema"),
        })
        self.check(
            "support-bundle-capabilities-schema",
            record.get("capabilities_schema") == "crab-cache-service.capabilities.v1",
            {"capabilities_schema": record.get("capabilities_schema")},
        )
        self.check(
            "support-bundle-capabilities-cache-limit-matches-live",
            self.int_value(record, "capabilities_max_cache_bytes")
            == self.int_value(capabilities, "max_cache_bytes"),
            {
                "support_capabilities_max_cache_bytes": record.get("capabilities_max_cache_bytes"),
                "live_max_cache_bytes": capabilities.get("max_cache_bytes"),
            },
        )
        self.check(
            "support-bundle-capabilities-object-limit-matches-live",
            self.int_value(record, "capabilities_max_object_bytes")
            == self.int_value(capabilities, "max_object_bytes"),
            {
                "support_capabilities_max_object_bytes": record.get("capabilities_max_object_bytes"),
                "live_max_object_bytes": capabilities.get("max_object_bytes"),
            },
        )
        self.check(
            "support-bundle-authz-schema",
            record.get("authz_schema") == "crab-cache-service.authz-check.v1",
            {"authz_schema": record.get("authz_schema")},
        )
        for action in ("read", "write", "dedup", "admin"):
            self.check(
                f"support-bundle-authz-{action}-allowed",
                record.get(f"authz_{action}") is True,
                {f"authz_{action}": record.get(f"authz_{action}")},
            )
        self.check("support-bundle-cache-hit-rate-positive", self.float_value(record, "cache_hit_rate") > 0, {
            "cache_hit_rate": record.get("cache_hit_rate"),
        })
        origin_fallback = self.float_value(record, "origin_fallback_rate")
        self.check("support-bundle-origin-fallback-rate-bounded", 0 <= origin_fallback < 1, {
            "origin_fallback_rate": record.get("origin_fallback_rate"),
        })
        self.check("support-bundle-push-warming-observed", self.int_value(record, "push_warming_writes") > 0, {
            "push_warming_writes": record.get("push_warming_writes"),
        })
        repair_records = self.report.get("cache_integrity_repairs", [])
        self.check(
            "support-bundle-integrity-repairs-observed",
            self.int_value(record, "integrity_repairs")
            >= (len(repair_records) if isinstance(repair_records, list) else 0),
            {
                "integrity_repairs": record.get("integrity_repairs"),
                "repair_records": len(repair_records) if isinstance(repair_records, list) else None,
            },
        )
        self.check("support-bundle-evictions-observed", self.int_value(record, "evicted_objects") > 0, {
            "evicted_objects": record.get("evicted_objects"),
        })
        self.check("support-bundle-cache-max-present", self.float_value(record, "cache_max_bytes") > 0, {
            "cache_max_bytes": record.get("cache_max_bytes"),
        })
        self.check(
            "support-bundle-origin-avoidance-metric-present",
            self.float_value(record, "origin_avoided_reads_total") > 0,
            {"origin_avoided_reads_total": record.get("origin_avoided_reads_total")},
        )
        self.check(
            "support-bundle-max-object-bytes-matches-request-limit",
            self.int_value(record, "max_object_bytes")
            == self.int_value(request_limit, "max_object_bytes"),
            {
                "support_max_object_bytes": record.get("max_object_bytes"),
                "request_limit_max_object_bytes": request_limit.get("max_object_bytes"),
            },
        )
        self.check(
            "support-bundle-admin-object-limit-matches-capabilities",
            self.int_value(record, "max_object_bytes")
            == self.int_value(record, "capabilities_max_object_bytes"),
            {
                "admin_max_object_bytes": record.get("max_object_bytes"),
                "capabilities_max_object_bytes": record.get("capabilities_max_object_bytes"),
            },
        )
        self.check(
            "support-bundle-metrics-object-limit-matches-admin",
            self.float_value(record, "cache_max_object_bytes")
            == float(self.int_value(record, "max_object_bytes")),
            {
                "cache_max_object_bytes": record.get("cache_max_object_bytes"),
                "max_object_bytes": record.get("max_object_bytes"),
            },
        )

    def verify_origin_outage(self) -> None:
        record = self.record("origin_outages", "origin-outage-cached-read-through")
        self.check("origin-outage-health-503", self.int_value(record, "health_status") == 503, {
            "health_status": record.get("health_status"),
        })
        self.check("origin-outage-live-200", self.int_value(record, "live_status") == 200, {
            "live_status": record.get("live_status"),
        })
        self.check(
            "origin-outage-warm-miss-recorded",
            self.int_value(record, "warm_status") == 200 and record.get("warm_cache_status") == "MISS",
            {"record": record},
        )
        self.check(
            "origin-outage-hot-full-hit",
            self.int_value(record, "hot_status") == 200 and record.get("hot_cache_status") == "HIT",
            {"record": record},
        )
        self.check(
            "origin-outage-hot-range-hit",
            self.int_value(record, "range_status") == 206 and record.get("range_cache_status") == "HIT",
            {"record": record},
        )
        self.check(
            "origin-outage-cold-miss-504",
            self.int_value(record, "cold_status") == 504 and record.get("cold_cache_status") == "",
            {"record": record},
        )
        self.check(
            "origin-outage-origin-flat-for-hot-hits",
            self.int_value(record, "hot_origin_gets_after_hot")
            == self.int_value(record, "hot_origin_gets_before_outage")
            and self.int_value(record, "hot_origin_gets_after_range")
            == self.int_value(record, "hot_origin_gets_before_outage"),
            {"record": record},
        )
        self.check(
            "origin-outage-origin-flat-for-cold-failure",
            self.int_value(record, "cold_origin_gets_after_cold")
            == self.int_value(record, "cold_origin_gets_before_outage"),
            {"record": record},
        )
        self.check(
            "origin-outage-total-origin-flat",
            self.int_value(record, "total_origin_gets_after_hot")
            == self.int_value(record, "total_origin_gets_before_outage")
            and self.int_value(record, "total_origin_gets_after_range")
            == self.int_value(record, "total_origin_gets_before_outage")
            and self.int_value(record, "total_origin_gets_after_cold")
            == self.int_value(record, "total_origin_gets_before_outage"),
            {"record": record},
        )
        self.check(
            "origin-outage-cache-hit-counters-increase",
            self.int_value(record, "cache_hits_after_outage")
            >= self.int_value(record, "cache_hits_before_outage") + 2,
            {"record": record},
        )
        self.check(
            "origin-outage-origin-fetch-counters-flat",
            self.int_value(record, "origin_fetches_after_outage")
            == self.int_value(record, "origin_fetches_before_outage"),
            {"record": record},
        )
        self.check(
            "origin-outage-body-lengths",
            self.int_value(record, "hot_body_len") > 0
            and self.int_value(record, "range_body_len") > 0
            and self.int_value(record, "cold_body_len") > 0,
            {"record": record},
        )

    def verify_origin_outage_support_bundle(self) -> None:
        record = self.record("support_bundles", "origin-outage")
        self.check(
            "origin-outage-support-bundle-schema",
            record.get("schema") == "cache-service.support-bundle",
            {"schema": record.get("schema")},
        )
        self.check(
            "origin-outage-support-bundle-health-degraded",
            record.get("health_ok") is False and self.int_value(record, "health_status") == 503,
            {
                "health_ok": record.get("health_ok"),
                "health_status": record.get("health_status"),
            },
        )
        self.check(
            "origin-outage-support-bundle-auth-probe-control-plane",
            record.get("auth_endpoint") == "/v1/capabilities",
            {"auth_endpoint": record.get("auth_endpoint")},
        )
        for probe_name in ("auth", "capabilities", "authz", "admin_stats", "metrics"):
            self.check(
                f"origin-outage-support-bundle-{probe_name}-probe-ok",
                record.get(f"{probe_name}_ok") is True
                and self.int_value(record, f"{probe_name}_status") == 200,
                {
                    f"{probe_name}_ok": record.get(f"{probe_name}_ok"),
                    f"{probe_name}_status": record.get(f"{probe_name}_status"),
                },
            )
        self.check(
            "origin-outage-support-bundle-cache-hit-rate-positive",
            self.float_value(record, "cache_hit_rate") > 0,
            {"cache_hit_rate": record.get("cache_hit_rate")},
        )
        self.check(
            "origin-outage-support-bundle-origin-avoidance-metric-present",
            self.float_value(record, "origin_avoided_reads_total") > 0,
            {"origin_avoided_reads_total": record.get("origin_avoided_reads_total")},
        )


def load_report(path: Path) -> dict[str, Any]:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise VerifyError(f"cannot read report {path}: {exc}") from exc
    except json.JSONDecodeError as exc:
        raise VerifyError(f"invalid report JSON {path}: {exc}") from exc
    if not isinstance(payload, dict):
        raise VerifyError("report root must be a JSON object")
    return payload


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_summary(path: Path, summary: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--output", type=Path, help="write verifier summary JSON")
    parser.add_argument(
        "--forbid-secret",
        action="append",
        default=[],
        help="additional literal secret that must not appear in the report JSON",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    summary: dict[str, Any]
    try:
        raw_text = args.report.read_text(encoding="utf-8")
        for secret in (*DEFAULT_FORBIDDEN_SECRETS, *args.forbid_secret):
            if secret and secret in raw_text:
                raise VerifyError(f"report contains forbidden secret literal: {secret!r}")
        report = load_report(args.report)
        summary = Verifier(report, args.report).verify()
    except VerifyError as exc:
        summary = {"status": "failed", "report": str(args.report), "error": str(exc)}
        if args.output is not None:
            write_summary(args.output, summary)
        print(f"FAILED cache-service smoke report verification: {exc}", file=sys.stderr)
        return 1

    summary["report"] = str(args.report)
    if args.output is not None:
        write_summary(args.output, summary)
    print("PASS cache-service smoke report verification")
    print(f"report: {args.report}")
    print(f"verified checks: {summary['verified_checks']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
