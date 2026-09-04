#!/usr/bin/env python3
"""Validate and render Crab's executable Git compatibility contract."""

from __future__ import annotations

import argparse
import hashlib
import itertools
import json
import subprocess
from datetime import datetime, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
MATRIX = ROOT / "crab/docs/architecture/git-capability-matrix.json"
DOC = ROOT / "crab/docs/architecture/git-capability-matrix.md"
ALLOWED_STATUS = {"supported", "preview", "unsupported"}


def load_matrix() -> dict:
    with MATRIX.open(encoding="utf-8") as handle:
        return json.load(handle)


def cells(matrix: dict) -> set[tuple[str, str, str, str, str]]:
    dimensions = matrix["dimensions"]
    return set(
        itertools.product(
            dimensions["git_versions"],
            dimensions["operating_systems"],
            dimensions["repository_modes"],
            dimensions["providers"],
            dimensions["operations"],
        )
    )


def profile_cells(profile: dict) -> set[tuple[str, str, str, str, str]]:
    return set(
        itertools.product(
            profile["git_versions"],
            profile["operating_systems"],
            profile["repository_modes"],
            profile["providers"],
            profile["operations"],
        )
    )


def validate(matrix: dict) -> None:
    if matrix.get("schema") != "crab.git-capability-matrix" or matrix.get("version") != "1.0":
        raise ValueError("matrix schema/version must be crab.git-capability-matrix 1.0")

    expected = cells(matrix)
    assigned: dict[tuple[str, str, str, str, str], str] = {}
    profile_ids: set[str] = set()
    for profile in matrix["profiles"]:
        if profile["id"] in profile_ids:
            raise ValueError(f"duplicate profile id {profile['id']}")
        profile_ids.add(profile["id"])
        if profile["status"] not in ALLOWED_STATUS:
            raise ValueError(f"{profile['id']}: invalid status {profile['status']}")
        evidence = profile.get("evidence", {})
        workflow = evidence.get("workflow")
        if not workflow or not (ROOT / workflow).is_file():
            raise ValueError(f"{profile['id']}: evidence workflow is missing")
        for cell in profile_cells(profile):
            if cell not in expected:
                raise ValueError(f"{profile['id']}: unknown capability cell {cell}")
            if cell in assigned:
                raise ValueError(
                    f"capability cell {cell} appears in both {assigned[cell]} and {profile['id']}"
                )
            assigned[cell] = profile["id"]

    missing = expected.difference(assigned)
    if missing:
        raise ValueError(f"{len(missing)} capability cells have no explicit status")

    checks = matrix.get("evidence_checks", {})
    if set(checks) != set(matrix["dimensions"]["operations"]):
        raise ValueError("every operation must declare its required report checks")
    if any(not names or len(names) != len(set(names)) for names in checks.values()):
        raise ValueError("required report checks must be nonempty and unique per operation")
    supported = [profile for profile in matrix["profiles"] if profile["status"] == "supported"]
    if not supported:
        raise ValueError("matrix must contain at least one supported profile")
    for profile in supported:
        evidence = profile["evidence"]
        runner = ROOT / evidence["runner"]
        if not runner.is_file():
            raise ValueError(f"{profile['id']}: evidence runner is missing")

    protocol = matrix["protocol_profile"]
    overlap = set(protocol["supported"]).intersection(protocol["unsupported"])
    if overlap:
        raise ValueError(f"protocol capabilities have conflicting status: {sorted(overlap)}")
    if protocol.get("unsupported_contract") != (
        "reject unsupported v2 requests before pack bytes; helper fallback only before handoff"
    ):
        raise ValueError("unsupported protocol contract must remain fail closed")


def validate_report(
    matrix: dict,
    report: dict,
    profile_id: str,
    git_version: str,
    actual_git_version: str,
    binary_sha256: str,
    source_revision: str,
    rollback_sha256: str,
) -> None:
    profile = next((item for item in matrix["profiles"] if item["id"] == profile_id), None)
    if profile is None or git_version not in profile["git_versions"]:
        raise ValueError("unknown profile or Git version")
    if report.get("schema") != "crab.protocol-v2-partial-clone-smoke" or report.get("version") != "1.1":
        raise ValueError("unsupported evidence report schema")
    if report.get("status") != "passed":
        raise ValueError("evidence report did not pass")
    timestamp = datetime.fromisoformat(report.get("updated_at", ""))
    age = (datetime.now(timezone.utc) - timestamp).total_seconds()
    if not -300 <= age <= 86400:
        raise ValueError("evidence report is stale or future-dated")
    provenance = report.get("provenance", {})
    provider = {"s3": "aws_s3"}.get(report.get("backend"), report.get("backend"))
    if (
        provider not in profile["providers"]
        or provenance.get("backend") != report.get("backend")
        or provenance.get("operating_system") not in profile["operating_systems"]
        or provenance.get("repository_mode") not in profile["repository_modes"]
    ):
        raise ValueError("evidence provider/platform/mode does not match profile")
    if provenance.get("git_version") != actual_git_version or (
        git_version != "current" and actual_git_version != f"git version {git_version}"
    ):
        raise ValueError("evidence Git version does not match the selected executable")
    if (
        provenance.get("crab_binary_sha256") != binary_sha256
        or provenance.get("crab_source_revision") != source_revision
        or provenance.get("crab_source_checkout_clean") is not True
        or provenance.get("crab_binary_matches_source_revision") is not True
    ):
        raise ValueError("evidence is not from the exact binary and clean source revision")
    if "rollback_client" in profile["operations"] and (
        provenance.get("rollback_crab_tag") != matrix["rollback_release"]
        or not rollback_sha256
        or provenance.get("rollback_binary_sha256") != rollback_sha256
    ):
        raise ValueError("rollback evidence is missing or from another binary")
    results = {}
    for check in report.get("checks", []):
        if check.get("name") in results:
            raise ValueError(f"duplicate report check: {check['name']}")
        if check.get("ok") is not True or check.get("detail", {}).get("skipped"):
            raise ValueError(f"failed or skipped report check: {check.get('name')}")
        results[check["name"]] = check
    required = {
        name for operation in profile["operations"] for name in matrix["evidence_checks"][operation]
    }
    missing = required.difference(results)
    if missing:
        raise ValueError(f"missing required evidence checks: {sorted(missing)}")
    filters = report.get("performance", {}).get("filter-matrix", {})
    if filters.get("client_capabilities", {}).get("object_type_filter") is True:
        for kind in ("tag", "commit", "tree", "blob"):
            name = f"filter-matrix-object-type-{kind}-protocol-and-promisor"
            if name not in results:
                raise ValueError(f"missing supported client filter check: {name}")


def render(matrix: dict) -> str:
    lines = [
        "# Git capability matrix",
        "",
        "This file is generated from `git-capability-matrix.json`. Run",
        "`python3 crab/scripts/verify_git_capability_matrix.py --write` after editing the matrix.",
        "",
        "| Profile | Git | OS | Provider | Operations | Status | Evidence |",
        "| --- | --- | --- | --- | ---: | --- | --- |",
    ]
    for profile in matrix["profiles"]:
        evidence = profile["evidence"]
        lines.append(
            "| {id} | {git} | {os} | {provider} | {operations} | {status} | `{workflow}` / `{job}` |".format(
                id=profile["id"],
                git=", ".join(profile["git_versions"]),
                os=", ".join(profile["operating_systems"]),
                provider=", ".join(profile["providers"]),
                operations=len(profile["operations"]),
                status=profile["status"],
                workflow=evidence["workflow"],
                job=evidence["job"],
            )
        )
    lines.extend(
        [
            "",
            "## Protocol boundary",
            "",
            f"Transport: `{matrix['protocol_profile']['transport']}`.",
            "",
            "Supported: " + ", ".join(f"`{item}`" for item in matrix["protocol_profile"]["supported"]) + ".",
            "",
            "Unsupported: " + ", ".join(f"`{item}`" for item in matrix["protocol_profile"]["unsupported"]) + ".",
            "",
            "Unsupported v2 requests are rejected before pack bytes. Helper transport negotiation may return `fallback` before handoff (for example, receive-pack takeover uses the ordinary helper push path); Crab never substitutes a complete fetch for a rejected v2 request or falls back after partial v2 output.",
            "",
            "`supported` declares mandatory release-gate cells, not a claim that an unverified checkout passed. The named workflow must validate a fresh report against these operation checks, the exact packaged binary, clean source SHA, Git executable, platform, provider, and pinned rollback binary. Missing or skipped checks fail the gate. `preview` is not a compatibility promise.",
            "",
        ]
    )
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--write", action="store_true")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--profile")
    parser.add_argument("--git-version")
    parser.add_argument("--git-bin")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--source-revision")
    parser.add_argument("--rollback-binary", type=Path)
    args = parser.parse_args()
    matrix = load_matrix()
    validate(matrix)
    if args.report:
        if not all((args.profile, args.git_version, args.git_bin, args.binary, args.source_revision, args.rollback_binary)):
            parser.error("--report requires profile, Git version/executable, binary, source revision, and rollback binary")
        def digest(path: Path) -> str:
            result = hashlib.sha256()
            with path.open("rb") as handle:
                for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                    result.update(chunk)
            return result.hexdigest()
        git_version = subprocess.check_output([args.git_bin, "--version"], text=True).strip()
        validate_report(
            matrix, json.loads(args.report.read_text(encoding="utf-8")), args.profile,
            args.git_version, git_version, digest(args.binary), args.source_revision,
            digest(args.rollback_binary),
        )
    rendered = render(matrix)
    if args.write:
        DOC.write_text(rendered, encoding="utf-8")
        return 0
    if not DOC.is_file() or DOC.read_text(encoding="utf-8") != rendered:
        raise SystemExit("generated Git capability documentation is stale; rerun with --write")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
