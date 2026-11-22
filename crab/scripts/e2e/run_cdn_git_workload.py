#!/usr/bin/env python3
"""Measure a native push and ordinary Git or LFS fetch on a dedicated CDN fixture."""

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

from collect_cdn_product_metrics import NATIVE_TARGET, ROUTE, ROUTE_FIELDS, TARGET, measure, measure_native
from run_cdn_transport_checks import approved_fixture, fixture_base, fixture_object_key, native_remote

from cdn_qualification import (
    CLI_BUILD_FIELDS, CLI_NATIVE_FIELDS, COMMIT, GIT_OID, GIT_PAYLOAD_SIZE, PROVIDER_FOR_EDGE, RUN_NUMBER, SHA256,
    cli_action_name, decode, deployment_component_digest,
    fields, is_direct_desktop_git_scope, is_direct_sdk_git_scope, is_direct_service_scope, lfs_object_path,
    matches, read_git_record, require, requires_cli_lfs_workload,
    requires_cli_workload, scope_parts,
    validate_git_publication, verified_publication, verify_read_candidate,
)
from public_inventory import ExportError, read_document


PAYLOAD = "cdn-qualification.bin"
QUALIFICATION_LOG = "crab::cdn::qualification"
QUALIFICATION_MESSAGE = "CDN qualification verified native target"
QUALIFICATION_TARGET_ENV = "CRAB_INTERNAL_CDN_QUALIFICATION_TARGET"
READ_LOG = "crab::cdn::qualification_read"
READ_MESSAGE = "CDN qualification verified read configuration"
PUBLIC_TRANSITION_TARGET = "crab_storage::cdn::public_transition"
GCS_VIEWER_LOG_TARGET = "crab_storage::cdn::gcs_viewer_log"


def run_command(
    command: list[str], label: str, root: Path, env: dict[str, str], content: bytes | None = None,
) -> subprocess.CompletedProcess[bytes]:
    try:
        result = subprocess.run(
            command, cwd=root, env=env, input=content, capture_output=True, timeout=300, check=False,
        )
    except subprocess.TimeoutExpired:
        raise ValueError(f"{label} timed out") from None
    require(result.returncode == 0, f"{label} failed with exit {result.returncode}")
    return result


def runner_platform() -> str:
    system = "windows" if sys.platform == "win32" else sys.platform
    machine = platform.machine().lower()
    architecture = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    return f"{system}-{architecture}"


def viewer_binding_sha256(fixture: dict) -> str:
    _, access, _, _, _ = scope_parts(fixture["scope"])
    base = fixture_base(fixture["base_url"])
    authority = base.netloc.lower()
    if base.port == 443:
        authority = authority.rsplit(":", 1)[0]
    return hashlib.sha256(f"{access}\nhttps://{authority}{base.path.rstrip('/')}".encode()).hexdigest()


def read_target_fingerprints(fixture: dict, *, edge: bool, export_sha256: str) -> dict[str, str]:
    return {
        "origin_sha256": hashlib.sha256(fixture["native_origin"].encode()).hexdigest(),
        "prefix_sha256": hashlib.sha256(f"{fixture['fixture_prefix']}/".encode()).hexdigest(),
        "binding_sha256": viewer_binding_sha256(fixture) if edge else "",
        "public_export_sha256": export_sha256,
    }


def verified_read(lines: list[bytes], expected: dict) -> dict:
    found = False
    for line in lines:
        if READ_LOG.encode() not in line:
            continue
        event = decode(line)
        if not isinstance(event, dict) or event.get("target") != READ_LOG:
            continue
        observed = fields(event.get("fields"), expected.keys() | {"message"}, "read configuration observation")
        require(observed["message"] == READ_MESSAGE
                and all(observed[key] == expected[key] for key in ("binding_sha256", "cache_root_sha256")),
                "fetch selected a different CDN binding or local cache root")
        require(observed["public_export_sha256"] == expected["public_export_sha256"],
                "fetch selected a different CDN public export approval")
        found = True
    require(found, "fetch did not confirm its CDN binding and local cache root")
    return expected


class NativeTargetGuard:
    def __init__(self, helper: Path, source: str, target: dict[str, str], expected: dict | None) -> None:
        with helper.open("rb") as binary:
            self.binary_sha256 = hashlib.file_digest(binary, "sha256").hexdigest()
        self.source = source
        self.platform = runner_platform()
        self.candidate = expected
        self.fingerprints = {
            "origin_sha256": hashlib.sha256(target["origin"].encode()).hexdigest(),
            "prefix_sha256": hashlib.sha256((target["repository_prefix"] + "/").encode()).hexdigest(),
        }
        if expected is not None:
            require(expected["binary_sha256"] == self.binary_sha256, "the Git helper differs from the native publication binary")

    def confirm(self, result: subprocess.CompletedProcess[bytes], label: str) -> None:
        found = False
        for line in result.stderr.splitlines():
            if QUALIFICATION_LOG.encode() not in line:
                continue
            event = decode(line)
            if not isinstance(event, dict) or event.get("target") != QUALIFICATION_LOG:
                continue
            record = fields(event.get("fields"), CLI_BUILD_FIELDS | CLI_NATIVE_FIELDS | {"message"}, "native target observation")
            require(record["message"] == QUALIFICATION_MESSAGE
                    and all(record[key] == value for key, value in self.fingerprints.items()),
                    f"{label} did not confirm the approved native origin and repository prefix")
            candidate = {key: record[key] for key in CLI_BUILD_FIELDS} | {"binary_sha256": self.binary_sha256}
            verify_read_candidate(candidate, self.source, self.platform)
            require(self.candidate is None or candidate == self.candidate, f"{label} ran a different CLI build")
            self.candidate = candidate
            found = True
        require(found, f"{label} did not confirm the approved native origin and repository prefix")


def run_git(*args: str, root: Path, env: dict[str, str], guard: NativeTargetGuard | None = None) -> subprocess.CompletedProcess[bytes]:
    label = f"git {args[0]}"
    result = run_command(["git", *args], label, root, env)
    if guard is not None:
        guard.confirm(result, label)
    return result


def lfs_pointer(digest: str, size: int = GIT_PAYLOAD_SIZE) -> bytes:
    return f"version https://git-lfs.github.com/spec/v1\noid sha256:{digest}\nsize {size}\n".encode()


def companion_crab(helper: Path) -> Path:
    candidate = helper.absolute().with_name("crab" + helper.suffix)
    require(candidate.is_file() and os.access(candidate, os.X_OK), "place the matched Crab binary alongside the Git helper")
    if not helper.samefile(candidate):
        with helper.open("rb") as git_helper, candidate.open("rb") as crab:
            require(hashlib.file_digest(git_helper, "sha256").digest() == hashlib.file_digest(crab, "sha256").digest(),
                    "Crab and its Git helper must be from the same build")
    return candidate


def git_environment(helper: Path, root: Path, native_target: dict[str, str]) -> dict[str, str]:
    # Use the selected release helper and fresh Git and Crab object caches.
    require(helper.is_file() and helper.name in {"git-remote-crab", "git-remote-crab.exe"},
            "select the candidate's git-remote-crab executable")
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    return env | {
        "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_EXEC_PATH": str(helper.absolute().parent),
        "GIT_TERMINAL_PROMPT": "0", "CRAB_CACHE_DIR": str(root / "cache"),
        "CRAB_LOG": (f"{TARGET}=debug,{NATIVE_TARGET}=debug,{QUALIFICATION_LOG}=debug,{READ_LOG}=debug,"
                     f"{PUBLIC_TRANSITION_TARGET}=debug,"
                     f"{GCS_VIEWER_LOG_TARGET}=trace"),
        QUALIFICATION_TARGET_ENV: json.dumps(native_target),
    }


def ref_tip(remote: str, ref: str, root: Path, env: dict[str, str], guard: NativeTargetGuard) -> str | None:
    result = run_git("ls-remote", "--refs", remote, ref, root=root, env=env, guard=guard)
    if not result.stdout:
        return None
    columns = result.stdout.strip().split(b"\t")
    require(len(columns) == 2 and columns[1] == ref.encode("ascii"), "unexpected remote ref advertisement")
    oid = columns[0].decode("ascii")
    require(matches(GIT_OID, oid), "remote ref advertisement contained an invalid commit")
    return oid


def publish(
    remote: str, provenance: dict[str, str], receipt: Path, root: Path, env: dict[str, str],
    crab: Path | None, guard: NativeTargetGuard, *, supplied_content: bytes | None = None,
) -> dict:
    ref = f"refs/heads/crab-cdn-qualification-{provenance['workflow_run_id']}-{provenance['workflow_run_attempt']}-{uuid4().hex[:12]}"
    content = supplied_content
    if content is None:
        content = hashlib.shake_256(f"{provenance['configuration_sha256']}/{ref}".encode()).digest(GIT_PAYLOAD_SIZE)
    sha256 = hashlib.sha256(content).hexdigest()
    run_git("init", "--template=", "--object-format=sha1", root=root, env=env)
    if crab is not None:
        pointer = run_command([str(crab), "lfs", "clean", PAYLOAD], "crab lfs clean", root, env, content).stdout
        require(pointer == lfs_pointer(sha256, len(content)), "crab lfs clean did not stage the expected fixture pointer")
        content = pointer
    (root / PAYLOAD).write_bytes(content)
    run_git("add", "--", PAYLOAD, root=root, env=env)
    run_git(
        "-c", "user.name=Crab CDN qualification", "-c", "user.email=qualification@example.invalid",
        "-c", "commit.gpgsign=false", "commit", "-m", "CDN qualification payload", root=root, env=env,
    )
    oid = run_git("rev-parse", "HEAD", root=root, env=env).stdout.strip().decode("ascii")
    require(matches(GIT_OID, oid), "writer did not create a valid fixture commit")
    # A read-only probe prevents an older helper that ignores the origin guard from writing.
    require(ref_tip(remote, ref, root, env, guard) is None, "the qualification ref already exists")
    record = provenance | {"schema": "crab.cdn-git-publication/v1", "ref": ref, "oid": oid,
                           "sha256": sha256, "published": False, "visible_at_unix_ms": None, "candidate": guard.candidate}
    # Keep a receipt before sending the push so a timed-out publication can be retired.
    with receipt.open("x", encoding="utf-8") as file:
        json.dump(record, file, sort_keys=True)
    run_git("push", f"--force-with-lease={ref}:", remote, f"HEAD:{ref}", root=root, env=env, guard=guard)
    require(ref_tip(remote, ref, root, env, guard) == oid, "new ref was not immediately visible after native push")
    record["published"] = True
    record["visible_at_unix_ms"] = time.time_ns() // 1_000_000
    with TemporaryDirectory(dir=receipt.parent, prefix=".crab-publish-") as staging:
        confirmed = Path(staging) / "receipt.json"
        confirmed.write_text(json.dumps(record, sort_keys=True) + "\n", encoding="utf-8")
        confirmed.replace(receipt)
    return {"visible_new_ref": True, "native_writes": 1}


def verify_transition_events(lines: list[bytes], scope: str, proof: dict, *, git_blob: str | None = None) -> None:
    edge, access, consumer, family, profile = scope_parts(scope)
    selected = (PROVIDER_FOR_EDGE[edge], access, consumer, family, profile)
    observations = set()
    for line in lines:
        if PUBLIC_TRANSITION_TARGET.encode() not in line:
            continue
        event = decode(line)
        if not isinstance(event, dict) or event.get("target") != PUBLIC_TRANSITION_TARGET:
            continue
        raw = event.get("fields")
        blob_field = {"git_blob_oid_sha256"} if isinstance(raw, dict) and "git_blob_oid_sha256" in raw else set()
        row = fields(raw, ROUTE_FIELDS | {"observation", "approval_sha256", "key_sha256"} | blob_field,
                     "public export product observation")
        require(row["message"] == "CDN public export observation"
                and row["observation"] in ("listed", "unlisted", "verified_edge", "verified_native")
                and (not blob_field or (row["family"] == "git"
                    and row["observation"] in ("verified_edge", "verified_native")
                    and matches(SHA256, row["git_blob_oid_sha256"]))),
                "invalid public export product observation")
        if tuple(row[key] for key in ROUTE) != selected or row["key_sha256"] != proof["key_sha256"]:
            continue
        require(row["approval_sha256"] == proof["receipt_sha256"], "fetch loaded a different public export receipt")
        observed = row["observation"]
        if observed == "verified_native" and git_blob is None:
            continue
        if observed.startswith("verified_") and git_blob is not None and row.get("git_blob_oid_sha256") != git_blob:
            continue
        observations.add(observed)
    expected = {"unlisted"} if proof["phase"] == "before" else {"listed", "verified_edge"}
    if git_blob is not None and proof["phase"] == "before":
        expected.add("verified_native")
    require(observations == expected, "fetch did not prove the selected object's public export transition")


def approved_public_export(directory: Path, fixture: dict) -> tuple:
    raw, _ = read_document(directory / "publication.json")
    digest = hashlib.sha256(raw).hexdigest()
    binding, objects, sealed_at = verified_publication(directory, digest)
    require(binding["source_origin"] == fixture["native_origin"]
            and binding["source_placement"] == fixture["source_placement"]
            and binding["export_origin"] == fixture["export_origin"] and binding["repository_prefix"] == fixture["fixture_prefix"]
            and binding["base_url"] == fixture["base_url"].rstrip("/"), "public export approval does not match the fixture")
    require(sealed_at <= time.time_ns() // 1_000_000, "public export approval must be sealed before the fetch")
    return digest, objects, sealed_at


def prepare_public_export(args: argparse.Namespace, fixture: dict, record: dict | None) -> tuple[str, dict | None]:
    _, access, _, family, _ = scope_parts(fixture["scope"])
    transition = any((args.transition_object, args.transition_output, args.before_transition))
    if transition:
        require(record is not None and access == "public" and args.action in ("fetch", "fetch-origin")
                and args.transition_output is not None,
                "public transition requires a publication receipt and transition output on a public fetch")
    approved = access == "public" and (transition or args.action in ("fetch", "fetch-warm-local"))
    require((args.export_approval is not None) == approved,
            "an export approval is required for a public edge fetch, warm-local fetch or transition")
    if not approved:
        return "", None
    digest, objects, sealed_at = approved_public_export(args.export_approval, fixture)
    if not transition:
        return digest, None
    path = args.transition_object
    if family == "lfs":
        expected = lfs_object_path(record["sha256"])
        require(path is None or path == expected, "transition LFS object must match the native publication")
        path = expected
        fixture = fixture | {"sha256": record["sha256"]}
    require(path, "public transition needs the exact object key")
    phase = "before" if args.action == "fetch-origin" else "after"
    require((args.before_transition is not None) == (phase == "after"), "the edge transition requires the native-only observation")
    key = fixture_object_key(fixture | {"object_path": path}, access, family)
    listed = any(obj.path == path for obj in objects)
    require(listed == (phase == "after"), "public transition object membership does not match the phase")
    require(not args.transition_output.exists() and not args.transition_output.is_symlink(),
            "public transition observation output must be new")
    return digest, {"phase": phase, "receipt_sha256": digest, "key_sha256": hashlib.sha256(key.encode()).hexdigest(),
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
        "edge_attempts": result["all_edge_attempts"], "binding_active": True, "reader": result["reader"],
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


def fetch_once(
    remote: str, record: dict[str, Any], root: Path, env: dict[str, str], proof: dict | None,
    crab: Path | None, guard: NativeTargetGuard, binding: str, public_export: str, *, primed: bool = False,
) -> dict:
    require(record["published"], "fetch requires a confirmed native publication")
    cache = Path(env["CRAB_CACHE_DIR"])
    lfs_payload = root / ".git" / lfs_object_path(record["sha256"])
    if primed and crab is not None:
        require(lfs_payload.is_file(), "warm-local LFS fetch needs the verified primer's cached object")
    else:
        require(cache.is_dir() if primed else not cache.exists(), "fetch needs the expected fresh or primed scratch cache")
    read = {"binding_sha256": binding, "cache_root_sha256": hashlib.sha256(env["CRAB_CACHE_DIR"].encode()).hexdigest(),
            "public_export_sha256": public_export}
    target = decode(env[QUALIFICATION_TARGET_ENV].encode()) | {"read": read}
    env = env | {QUALIFICATION_TARGET_ENV: json.dumps(target)}
    mode = ("--bare",) if crab is None else ()
    if not (primed and crab is not None):
        run_git("init", *mode, "--template=", "--object-format=sha1", root=root, env=env)
    if crab is not None:
        if not primed:
            run_git("remote", "add", "origin", remote, root=root, env=env)
            run_git("fetch", "--no-tags", "--no-auto-maintenance", remote, record["ref"], root=root, env=env, guard=guard)
            require(not lfs_payload.exists(), "LFS control needs an empty local payload cache")
        pointer = run_git("cat-file", "blob", f"FETCH_HEAD:{PAYLOAD}", root=root, env=env).stdout
        require(pointer == lfs_pointer(record["sha256"]), "native ref does not contain the expected LFS pointer")
    # Use a new cache and exact refspec so another fetched ref cannot supply the bytes.
    started_at = time.time_ns() // 1_000_000
    started = time.monotonic_ns()
    if crab is None:
        run = run_git("fetch", "--no-tags", "--no-auto-maintenance", remote, record["ref"], root=root, env=env, guard=guard)
    else:
        run = run_command([str(crab), "lfs", "fetch", "origin", record["oid"]], "crab lfs fetch", root, env)
        guard.confirm(run, "crab lfs fetch")
    elapsed = max(1, (time.monotonic_ns() - started) // 1_000_000)
    finished_at = time.time_ns() // 1_000_000
    actual = run_git("rev-parse", "FETCH_HEAD", root=root, env=env).stdout.strip().decode("ascii")
    require(actual == record["oid"], "fetch resolved a different commit")
    if crab is None:
        blob = run_git("cat-file", "blob", f"FETCH_HEAD:{PAYLOAD}", root=root, env=env).stdout
    else:
        with lfs_payload.open("rb") as payload:
            blob = payload.read(GIT_PAYLOAD_SIZE + 1)
    observed = hashlib.sha256(blob).hexdigest()
    require(len(blob) == GIT_PAYLOAD_SIZE and observed == record["sha256"], "fetched bytes differ from the writer payload")
    lines = run.stderr.splitlines()
    edge_responses = {}
    lfs_key = None
    if crab is not None:
        physical_key = f"{target['repository_prefix']}/{lfs_object_path(record['sha256'])}"
        lfs_key = hashlib.sha256(physical_key.encode()).hexdigest()
    metrics = measure(lines, record["scope"], edge_responses=edge_responses, expected_lfs_key=lfs_key)
    if proof is not None:
        verify_transition_events(lines, record["scope"], proof)
    action = cli_action_name(record["scope"])
    return {
        "schema": f"crab.cdn-{action}/v1", "name": action, "expected_sha256": record["sha256"],
        "observed_sha256": observed, "expected_bytes": GIT_PAYLOAD_SIZE, "observed_bytes": len(blob),
        "native_writes": 1, "visible_new_ref": True, "elapsed_ms": elapsed,
        "publication_oid": record["oid"], "started_at_unix_ms": started_at, "finished_at_unix_ms": finished_at,
        "platform": guard.platform, "candidate": guard.candidate,
        "reader": verified_read(lines, read) | guard.fingerprints, "edge_responses": edge_responses,
        "native_traffic": measure_native(lines, record["scope"]),
    } | metrics


def require_edge_fetch(result: dict) -> None:
    require(result["observed_edge_gets"] > 0, "fetch produced no completed edge GET for the approved scope")
    require(result["verified_edge_payloads"] > 0, "fetch did not accept a verified edge payload for the approved scope")


def fetch(
    action: str, remote: str, record: dict[str, Any], root: Path, env: dict[str, str], proof: dict | None,
    crab: Path | None, guard: NativeTargetGuard, approved_binding: str, public_export: str,
) -> dict:
    if action == "fetch-warm-local":
        primer = root / "primer"
        primer.mkdir()
        require_edge_fetch(fetch_once(remote, record, primer, env, None, crab, guard, approved_binding, public_export))
        # LFS stores verified payloads in the worktree's Git common directory.
        # The Git pack benchmark still starts with a fresh object database.
        measured = primer if crab is not None else root / "measured"
        if crab is None:
            measured.mkdir()
        result = fetch_once(remote, record, measured, env, None, crab, guard, approved_binding, public_export, primed=True)
        require(result["edge_attempts"] == 0 and result["verified_edge_payloads"] == 0
                and result["local_payload_hits"] > 0 and (crab is None or result["all_edge_attempts"] == 0),
                "warm-local fetch must use a verified local payload without an edge request for this scope")
        return result | {"arm": "warm_local"}
    binding = "" if action == "fetch-origin" and proof is None else approved_binding
    result = fetch_once(remote, record, root, env, proof, crab, guard, binding, public_export)
    if action == "fetch-origin":
        require(result["all_edge_attempts"] == 0, "origin control observed edge traffic")
        return result | {"arm": "origin"}
    require_edge_fetch(result)
    return result | {"arm": "edge"}


def retire(remote: str, record: dict[str, Any], root: Path, env: dict[str, str], guard: NativeTargetGuard) -> dict:
    run_git("init", "--bare", "--template=", "--object-format=sha1", root=root, env=env)
    tip = ref_tip(remote, record["ref"], root, env, guard)
    if tip is not None:
        require(tip == record["oid"], "qualification ref changed after publication")
        ref = record["ref"]
        run_git("push", f"--force-with-lease={ref}:{tip}", remote, f":{ref}", root=root, env=env, guard=guard)
        require(ref_tip(remote, ref, root, env, guard) is None, "qualification ref remains after retirement")
    return {"schema": "crab.cdn-git-retirement/v1", "ref_retired": True,
            "retired_at_unix_ms": time.time_ns() // 1_000_000, "candidate": guard.candidate}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("inspect", "publish", "fetch", "fetch-origin", "fetch-warm-local", "retire"))
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--receipt", type=Path, help="required for publication, fetch and retirement")
    parser.add_argument("--scratch", required=True, type=Path)
    parser.add_argument("--helper", required=True, type=Path)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--transition-object", help="exact repository-relative immutable key selected by the Git fetch")
    parser.add_argument("--export-approval", type=Path, help="directory with the active public inventory and receipt")
    parser.add_argument("--transition-output", type=Path, help="retain an observed public transition phase")
    parser.add_argument("--before-transition", type=Path, help="native-only observation to pair with this edge fetch")
    parser.add_argument("--hydration-pointer", type=Path)
    parser.add_argument("--hydration-pointer-sha256")
    parser.add_argument("--service-lfs-payload", type=Path, help="fixture-pinned LFS bytes for native service publication")
    args = parser.parse_args()
    try:
        require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id)
                and matches(RUN_NUMBER, args.run_attempt), "exact workflow run provenance is required")
        require(args.action == "inspect" or args.receipt is not None, "this action requires a publication receipt path")
        fixture = approved_fixture(args.fixture, args.configuration_sha256)
        scope = fixture["scope"]
        edge, access, _, family, _ = scope_parts(scope)
        service = is_direct_service_scope(scope)
        native_publication = is_direct_sdk_git_scope(scope) or is_direct_desktop_git_scope(scope) or service
        require(requires_cli_workload(scope)
                or (native_publication and args.action in ("publish", "retire") and fixture["view_suffix"] == ""),
                "workload needs a direct CLI scope or a native publication for a direct SDK, desktop, or service scope")
        content = None
        if args.hydration_pointer is not None or args.hydration_pointer_sha256 is not None:
            require(is_direct_sdk_git_scope(fixture["scope"]) and access == "private" and args.action == "publish"
                    and fixture["view_suffix"] == "" and args.hydration_pointer is not None
                    and matches(SHA256, args.hydration_pointer_sha256),
                    "SDK hydration pointer publication requires the approved native source fixture and pointer digest")
            content = read_git_record(args.hydration_pointer, limit=4096)
            require(content and hashlib.sha256(content).hexdigest() == args.hydration_pointer_sha256,
                    "hydration pointer differs from the approved workload")
        service_lfs_publish = service and family == "lfs" and args.action == "publish"
        require((args.service_lfs_payload is not None) == service_lfs_publish,
                "native service LFS publication requires the exact approved fixture payload")
        if service_lfs_publish:
            content = read_git_record(args.service_lfs_payload, limit=fixture["size"])
            require(len(content) == fixture["size"] and hashlib.sha256(content).hexdigest() == fixture["sha256"],
                    "service LFS payload differs from the approved fixture")
        crab = companion_crab(args.helper) if requires_cli_lfs_workload(scope) or service_lfs_publish else None
        remote = native_remote(fixture, edge)
        approved_binding = viewer_binding_sha256(fixture)
        provenance = {
            "scope": fixture["scope"], "configuration_sha256": args.configuration_sha256,
            "source_sha": args.source_sha, "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt,
        }
        with TemporaryDirectory(prefix="crab-cdn-git-", dir=args.scratch) as temporary:
            root = Path(temporary).resolve()
            target = {"origin": fixture["native_origin"], "repository_prefix": fixture["fixture_prefix"]}
            env = git_environment(args.helper, root, target)
            if args.action in ("inspect", "publish"):
                prepare_public_export(args, fixture, None)
                guard = NativeTargetGuard(args.helper, args.source_sha, target, None)
                if args.action == "inspect":
                    run_git("init", "--bare", "--template=", "--object-format=sha1", root=root, env=env)
                    ref_tip(remote, "refs/heads/crab-cdn-qualification-build-inspection", root, env, guard)
                    result = {"candidate": guard.candidate, "platform": guard.platform,
                              "viewer_binding_sha256": approved_binding}
                else:
                    result = publish(remote, provenance, args.receipt, root, env, crab, guard,
                                     supplied_content=content) | {"candidate": guard.candidate}
            else:
                raw = read_git_record(args.receipt)
                record = validate_git_publication(raw, provenance)
                guard = NativeTargetGuard(args.helper, args.source_sha, target, record["candidate"])
                public_export, proof = prepare_public_export(args, fixture, record)
                if args.action == "retire":
                    result = retire(remote, record, root, env, guard)
                else:
                    result = fetch(args.action, remote, record, root, env, proof, crab, guard, approved_binding, public_export)
                    if proof is not None:
                        write_transition(proof, record, result, args.transition_output, args.before_transition)
                        if proof["phase"] == "after":
                            result["transition_key_sha256"] = proof["key_sha256"]
                result["publication_sha256"] = hashlib.sha256(raw).hexdigest()
    except (ExportError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(provenance | result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
