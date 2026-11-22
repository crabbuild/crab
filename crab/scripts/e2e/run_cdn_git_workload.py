#!/usr/bin/env python3
"""Measure a native push and normal Git fetch on a dedicated CDN fixture."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import subprocess
import sys
import time
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
from uuid import uuid4

from collect_cdn_product_metrics import ROUTE, ROUTE_FIELDS, TARGET, measure
from run_cdn_transport_checks import approved_fixture, fixture_object_key, native_remote

from cdn_qualification import (
    COMMIT, GIT_OID, GIT_PAYLOAD_SIZE, PROVIDER_FOR_EDGE, RUN_NUMBER, decode, deployment_component_digest,
    fields, matches, read_git_record, require, requires_git_workload, scope_parts, validate_git_publication,
    verified_publication,
)
from public_inventory import ExportError, read_document


PAYLOAD = "cdn-qualification.bin"
ORIGIN_TARGET = "crab::cdn::qualification"
ORIGIN_EVENT = {"message": "CDN qualification verified native origin"}
PUBLIC_TRANSITION_TARGET = "crab_storage::cdn::public_transition"


def run_git(*args: str, root: Path, env: dict[str, str]) -> subprocess.CompletedProcess[bytes]:
    try:
        result = subprocess.run(
            ["git", *args], cwd=root, env=env, capture_output=True, timeout=300, check=False,
        )
    except subprocess.TimeoutExpired:
        raise ValueError(f"git {args[0]} timed out") from None
    require(result.returncode == 0, f"git {args[0]} failed with exit {result.returncode}")
    if args[0] in {"ls-remote", "push", "fetch"}:
        observations = (decode(line) for line in result.stderr.splitlines() if ORIGIN_TARGET.encode() in line)
        require(any(isinstance(event, dict) and event.get("target") == ORIGIN_TARGET
                    and event.get("fields") == ORIGIN_EVENT for event in observations),
                f"git {args[0]} did not confirm the approved native origin through the qualification helper")
    return result


def git_environment(helper: Path, root: Path, native_origin: str) -> dict[str, str]:
    # Use the selected release helper and fresh Git and Crab object caches.
    require(helper.is_file() and helper.name in {"git-remote-crab", "git-remote-crab.exe"},
            "select the qualification build's git-remote-crab executable")
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    return env | {
        "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_EXEC_PATH": str(helper.absolute().parent),
        "GIT_TERMINAL_PROMPT": "0", "CRAB_CACHE_DIR": str(root / "cache"),
        "CRAB_LOG": f"{TARGET}=debug,{ORIGIN_TARGET}=debug,{PUBLIC_TRANSITION_TARGET}=debug",
        "CRAB_INTERNAL_CDN_QUALIFICATION_ORIGIN": native_origin,
    }


def ref_tip(remote: str, ref: str, root: Path, env: dict[str, str]) -> str | None:
    result = run_git("ls-remote", "--refs", remote, ref, root=root, env=env)
    if not result.stdout:
        return None
    columns = result.stdout.strip().split(b"\t")
    require(len(columns) == 2 and columns[1] == ref.encode("ascii"), "unexpected remote ref advertisement")
    oid = columns[0].decode("ascii")
    require(matches(GIT_OID, oid), "remote ref advertisement contained an invalid commit")
    return oid


def publish(remote: str, provenance: dict[str, str], receipt: Path, root: Path, env: dict[str, str]) -> dict:
    ref = f"refs/heads/crab-cdn-qualification-{provenance['workflow_run_id']}-{provenance['workflow_run_attempt']}-{uuid4().hex[:12]}"
    content = hashlib.shake_256(f"{provenance['configuration_sha256']}/{ref}".encode()).digest(GIT_PAYLOAD_SIZE)
    run_git("init", "--template=", "--object-format=sha1", root=root, env=env)
    (root / PAYLOAD).write_bytes(content)
    run_git("add", "--", PAYLOAD, root=root, env=env)
    run_git(
        "-c", "user.name=Crab CDN qualification", "-c", "user.email=qualification@example.invalid",
        "-c", "commit.gpgsign=false", "commit", "-m", "CDN qualification payload", root=root, env=env,
    )
    oid = run_git("rev-parse", "HEAD", root=root, env=env).stdout.strip().decode("ascii")
    require(matches(GIT_OID, oid), "writer did not create a valid fixture commit")
    # A read-only probe prevents an older helper that ignores the origin guard from writing.
    require(ref_tip(remote, ref, root, env) is None, "the qualification ref already exists")
    record = provenance | {"schema": "crab.cdn-git-publication/v1", "ref": ref, "oid": oid,
                           "sha256": hashlib.sha256(content).hexdigest(), "published": False, "visible_at_unix_ms": None}
    # Keep a receipt before sending the push so a timed-out publication can be retired.
    with receipt.open("x", encoding="utf-8") as file:
        json.dump(record, file, sort_keys=True)
    run_git("push", f"--force-with-lease={ref}:", remote, f"HEAD:{ref}", root=root, env=env)
    require(ref_tip(remote, ref, root, env) == oid, "new ref was not immediately visible after native push")
    record["published"] = True
    record["visible_at_unix_ms"] = time.time_ns() // 1_000_000
    with TemporaryDirectory(dir=receipt.parent, prefix=".crab-publish-") as staging:
        confirmed = Path(staging) / "receipt.json"
        confirmed.write_text(json.dumps(record, sort_keys=True) + "\n", encoding="utf-8")
        confirmed.replace(receipt)
    return {"visible_new_ref": True, "native_writes": 1}


def verify_transition_events(lines: list[bytes], scope: str, proof: dict) -> None:
    edge, access, consumer, family, profile = scope_parts(scope)
    selected = (PROVIDER_FOR_EDGE[edge], access, consumer, family, profile)
    observations = set()
    for line in lines:
        if PUBLIC_TRANSITION_TARGET.encode() not in line:
            continue
        event = decode(line)
        if not isinstance(event, dict) or event.get("target") != PUBLIC_TRANSITION_TARGET:
            continue
        row = fields(event.get("fields"), ROUTE_FIELDS | {"observation", "approval_sha256", "key_sha256"},
                     "public export product observation")
        require(row["message"] == "CDN public export observation"
                and row["observation"] in ("listed", "unlisted", "verified_edge"),
                "invalid public export product observation")
        if tuple(row[key] for key in ROUTE) != selected or row["key_sha256"] != proof["key_sha256"]:
            continue
        require(row["approval_sha256"] == proof["receipt_sha256"], "Git fetch loaded a different public export receipt")
        observations.add(row["observation"])
    expected = {"unlisted"} if proof["phase"] == "before" else {"listed", "verified_edge"}
    require(observations == expected, "Git fetch did not prove the selected object's public export transition")


def prepare_transition(args: argparse.Namespace, fixture: dict) -> dict | None:
    if not any((args.transition_object, args.export_approval, args.transition_output, args.before_transition)):
        return None
    _, access, _, family, _ = scope_parts(fixture["scope"])
    require(access == "public" and args.action in ("fetch", "fetch-origin") and args.transition_object
            and args.export_approval is not None and args.transition_output is not None,
            "public transition needs the object, export approval and transition output on a public Git fetch")
    phase = "before" if args.action == "fetch-origin" else "after"
    require((args.before_transition is not None) == (phase == "after"), "the edge transition requires the native-only observation")
    key = fixture_object_key(fixture | {"object_path": args.transition_object}, access, family)
    raw, _ = read_document(args.export_approval / "publication.json")
    digest = hashlib.sha256(raw).hexdigest()
    binding, objects, sealed_at = verified_publication(args.export_approval, digest)
    require(binding["source_origin"] == fixture["native_origin"]
            and binding["source_placement"] == fixture["source_placement"]
            and binding["export_origin"] == fixture["export_origin"] and binding["repository_prefix"] == fixture["fixture_prefix"]
            and binding["base_url"] == fixture["base_url"].rstrip("/"), "public transition approval does not match the fixture")
    listed = any(obj.path == args.transition_object for obj in objects)
    require(listed == (phase == "after"), "public transition object membership does not match the phase")
    require(not args.transition_output.exists() and not args.transition_output.is_symlink(),
            "public transition observation output must be new")
    return {"phase": phase, "receipt_sha256": digest, "key_sha256": hashlib.sha256(key.encode()).hexdigest(),
            "sealed_at_unix_ms": sealed_at}


def write_transition(proof: dict, record: dict, result: dict, output: Path, before: Path | None) -> None:
    phase = proof["phase"]
    reference = deployment_component_digest({"ref": record["ref"], "oid": record["oid"]})
    identity = {key: record[key] for key in ("scope", "source_sha", "configuration_sha256", "workflow_run_id", "workflow_run_attempt")}
    common = identity | {
        "schema": "crab.cdn-public-transition/v1", "platform": result["platform"], "key_sha256": proof["key_sha256"],
        "native_publication": {"reference_sha256": reference, "visible_at_unix_ms": record["visible_at_unix_ms"]},
    }
    observation = {
        "action": result["name"], "receipt_sha256": proof["receipt_sha256"], "key_sha256": proof["key_sha256"],
        "reference_sha256": reference, "result_sha256": result["observed_sha256"], "result_bytes": result["observed_bytes"],
        "started_at_unix_ms": result["started_at_unix_ms"], "finished_at_unix_ms": result["finished_at_unix_ms"],
        "edge_attempts": result["all_edge_attempts"], "binding_active": True,
        "verified_edge_key_sha256": proof["key_sha256"] if phase == "after" else None,
    }
    trace = common | {phase: observation}
    if before is None:
        require(proof["sealed_at_unix_ms"] <= record["visible_at_unix_ms"] <= result["started_at_unix_ms"],
                "prior public export must be sealed before the new native publication")
    else:
        first = fields(decode(read_git_record(before)), common.keys() | {"before"}, "native-only transition observation")
        require(all(first[key] == value for key, value in common.items()),
                "edge transition must follow the matching native-only observation")
        initial = fields(first["before"], observation.keys(), "native-only product observation")
        require(initial["finished_at_unix_ms"] < proof["sealed_at_unix_ms"] <= result["started_at_unix_ms"],
                "public export must be sealed between the native-only and edge fetches")
        trace["before"] = initial
    with output.open("x", encoding="utf-8") as destination:
        json.dump(trace, destination, sort_keys=True)
        destination.write("\n")


def fetch_once(remote: str, record: dict[str, Any], root: Path, env: dict[str, str], proof: dict | None = None) -> dict:
    require(record["published"], "fetch requires a confirmed native publication")
    run_git("init", "--bare", "--template=", "--object-format=sha1", root=root, env=env)
    # Use a new cache and exact refspec so another fetched ref cannot supply the bytes.
    started_at = time.time_ns() // 1_000_000
    started = time.monotonic_ns()
    run = run_git("fetch", "--no-tags", "--no-auto-maintenance", remote, record["ref"], root=root, env=env)
    elapsed = max(1, (time.monotonic_ns() - started) // 1_000_000)
    finished_at = time.time_ns() // 1_000_000
    actual = run_git("rev-parse", "FETCH_HEAD", root=root, env=env).stdout.strip().decode("ascii")
    require(actual == record["oid"], "fetch resolved a different commit")
    blob = run_git("cat-file", "blob", f"FETCH_HEAD:{PAYLOAD}", root=root, env=env).stdout
    observed = hashlib.sha256(blob).hexdigest()
    require(len(blob) == GIT_PAYLOAD_SIZE and observed == record["sha256"], "fetched bytes differ from the writer payload")
    lines = run.stderr.splitlines()
    metrics = measure(lines, record["scope"])
    if proof is not None:
        verify_transition_events(lines, record["scope"], proof)
    system = "windows" if sys.platform == "win32" else sys.platform
    machine = platform.machine().lower()
    architecture = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    return {
        "schema": "crab.cdn-git-fetch/v1", "name": "git-fetch", "expected_sha256": record["sha256"],
        "observed_sha256": observed, "expected_bytes": GIT_PAYLOAD_SIZE, "observed_bytes": len(blob),
        "native_writes": 1, "visible_new_ref": True, "elapsed_ms": elapsed,
        "publication_oid": record["oid"], "started_at_unix_ms": started_at, "finished_at_unix_ms": finished_at,
        "platform": f"{system}-{architecture}",
    } | metrics


def require_edge_fetch(result: dict) -> None:
    require(result["observed_edge_gets"] > 0, "fetch produced no completed edge GET for the approved scope")
    require(result["verified_edge_payloads"] > 0, "fetch did not accept a verified edge payload for the approved scope")


def fetch(action: str, remote: str, record: dict[str, Any], root: Path, env: dict[str, str], proof: dict | None = None) -> dict:
    if action == "fetch-warm-local":
        primer = root / "primer"
        primer.mkdir()
        require_edge_fetch(fetch_once(remote, record, primer, env))
        # The cache is shared, but the measured Git object database starts empty.
        measured = root / "measured"
        measured.mkdir()
        result = fetch_once(remote, record, measured, env)
        require(result["edge_attempts"] == 0 and result["verified_edge_payloads"] == 0
                and result["local_payload_hits"] > 0,
                "warm-local fetch must use a verified local pack without an edge request for this scope")
        return result | {"arm": "warm_local"}
    result = fetch_once(remote, record, root, env, proof)
    if action == "fetch-origin":
        require(result["all_edge_attempts"] == 0, "origin control observed edge traffic")
        return result | {"arm": "origin"}
    require_edge_fetch(result)
    return result | {"arm": "edge"}


def retire(remote: str, record: dict[str, Any], root: Path, env: dict[str, str]) -> dict:
    run_git("init", "--bare", "--template=", "--object-format=sha1", root=root, env=env)
    tip = ref_tip(remote, record["ref"], root, env)
    if tip is not None:
        require(tip == record["oid"], "qualification ref changed after publication")
        ref = record["ref"]
        run_git("push", f"--force-with-lease={ref}:{tip}", remote, f":{ref}", root=root, env=env)
        require(ref_tip(remote, ref, root, env) is None, "qualification ref remains after retirement")
    return {"schema": "crab.cdn-git-retirement/v1", "ref_retired": True,
            "retired_at_unix_ms": time.time_ns() // 1_000_000}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("publish", "fetch", "fetch-origin", "fetch-warm-local", "retire"))
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--receipt", required=True, type=Path)
    parser.add_argument("--scratch", required=True, type=Path)
    parser.add_argument("--helper", required=True, type=Path)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--transition-object", help="exact repository-relative immutable key selected by the Git fetch")
    parser.add_argument("--export-approval", type=Path, help="directory with the active public inventory and receipt")
    parser.add_argument("--transition-output", type=Path, help="retain an observed public transition phase")
    parser.add_argument("--before-transition", type=Path, help="native-only observation to pair with this edge fetch")
    args = parser.parse_args()
    try:
        require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
                and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        edge, _, _, _, _ = scope_parts(fixture["scope"])
        require(requires_git_workload(fixture["scope"]),
                "Git workload requires a direct CLI Git or capsule scope")
        proof = prepare_transition(args, fixture)
        remote = native_remote(fixture, edge)
        provenance = {
            "scope": fixture["scope"], "configuration_sha256": args.configuration_sha256,
            "source_sha": args.source_sha, "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
        }
        with TemporaryDirectory(prefix="crab-cdn-git-", dir=args.scratch) as temporary:
            root = Path(temporary).resolve()
            env = git_environment(args.helper, root, fixture["native_origin"])
            if args.action == "publish":
                result = publish(remote, provenance, args.receipt, root, env)
            else:
                raw = read_git_record(args.receipt)
                record = validate_git_publication(raw, provenance)
                if args.action == "retire":
                    result = retire(remote, record, root, env)
                else:
                    result = fetch(args.action, remote, record, root, env, proof)
                    if proof is not None:
                        write_transition(proof, record, result, args.transition_output, args.before_transition)
                result["publication_sha256"] = hashlib.sha256(raw).hexdigest()
    except (ExportError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(provenance | result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
