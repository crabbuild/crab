#!/usr/bin/env python3
"""Read a published CDN fixture through the ordinary desktop sidecar RPC stream."""

from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import json
import os
import subprocess
import sys
import time
import tomllib
from contextlib import suppress
from pathlib import Path
from tempfile import TemporaryDirectory, TemporaryFile

from collect_cdn_product_metrics import NATIVE_TARGET, TARGET, measure, measure_native
from run_cdn_git_workload import (
    GCS_VIEWER_LOG_TARGET, PAYLOAD, PUBLIC_TRANSITION_TARGET, runner_platform, verify_transition_events,
)
from run_cdn_sdk_workload import READ_TARGET, binding_for, json_record, verified_target
from run_cdn_transport_checks import approved_fixture

from cdn_qualification import (
    CLI_BUILD_FIELDS, COMMIT, GIT_PAYLOAD_SIZE, RUN_NUMBER, decode, fields, is_direct_desktop_git_scope, matches,
    read_git_record, require, scope_parts, validate_desktop_git_read, validate_git_publication, verify_read_candidate,
)
from public_inventory import ExportError


MAX_TRACE_BYTES = 8 * 1024 * 1024
RPC_FRAME_LIMIT = 512 * 1024


def sidecar_candidate(sidecar: Path, expected_digest: str, source: str, platform: str) -> dict:
    with sidecar.open("rb") as executable:
        digest = hashlib.file_digest(executable, "sha256").hexdigest()
    require(digest == expected_digest, "desktop executable differs from the preregistered build digest")
    result = subprocess.run([str(sidecar), "--cdn-build-info"], stdin=subprocess.DEVNULL,
                            capture_output=True, timeout=15, check=False)
    require(result.returncode == 0 and len(result.stdout) <= 4096, "could not inspect the desktop executable")
    build = fields(decode(result.stdout), CLI_BUILD_FIELDS, "desktop sidecar build metadata")
    return verify_read_candidate(build | {"binary_sha256": digest}, source, platform, consumer="desktop sidecar")


def read_configuration(fixture: dict, binding: dict) -> None:
    # Inspect the same trusted per-user document that ordinary remote browsing opens.
    config_path = Path.home() / ".config/crab/config.toml"
    try:
        config = tomllib.loads(read_git_record(config_path, limit=64 * 1024).decode())
    except (OSError, ValueError):
        raise ValueError("desktop qualification requires a valid dedicated worker user configuration") from None
    expected = {"origin": fixture["native_origin"], "repo_prefix": fixture["fixture_prefix"],
                "base_url": fixture["base_url"], "access": binding["access"]}
    if binding["access"] == "private":
        expected["approval_sha256"] = binding["approval_sha256"]
        auth = config.get("auth")
        require(isinstance(auth, dict) and all(auth.get(key) == value for key, value in {
            "provider": "crab-auth", "auth_endpoint": binding["endpoint"], "issuer_url": binding["oidc_issuer"],
            "client_id": binding["oidc_client_id"], "token_cache_path": binding["token_cache_directory"],
        }.items()), "desktop worker does not use the approved private issuer identity")
    else:
        expected |= {"export_origin": binding["export_origin"], "publication": binding["publication"]}
    entries = config.get("cdn")
    require(isinstance(entries, list), "desktop worker requires the approved CDN binding")
    selected = [entry for entry in entries if isinstance(entry, dict) and entry.get("origin") == expected["origin"]
                and entry.get("repo_prefix") == expected["repo_prefix"]]
    require(len(selected) == 1, "desktop worker requires one binding for the approved CDN fixture")
    actual = {"access": "private"} | selected[0]
    require(actual == {key: value for key, value in expected.items() if value is not None},
            "desktop user configuration differs from the approved CDN binding for this arm")


def desktop_url(fixture: dict) -> str:
    origin = fixture["native_origin"]
    if origin.startswith("https://"):
        origin = "az://" + origin.removeprefix("https://").replace(".blob.core.windows.net/", "/", 1)
    return f"{origin}/{fixture['fixture_prefix']}"


async def run_sidecar(command: list[str], root: Path, env: dict[str, str], open_request: tuple[str, dict],
                      read_request: dict, *, repeat: bool = False) -> list[tuple[dict, dict, list[bytes]]]:
    with TemporaryFile(dir=root) as trace:
        process = await asyncio.create_subprocess_exec(*command, cwd=root, env=env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=trace, limit=RPC_FRAME_LIMIT)
        stdin, stdout = process.stdin, process.stdout

        async def rpc(sequence: int, method: str, params: dict) -> dict:
            request = {"jsonrpc": "2.0", "id": sequence, "method": method, "params": params}
            stdin.write(json_record(request).encode())
            await stdin.drain()
            while line := await stdout.readline():
                frame = decode(line)
                require(isinstance(frame, dict) and frame.get("jsonrpc") == "2.0", "malformed sidecar RPC frame")
                if "id" not in frame and isinstance(frame.get("method"), str):
                    continue
                require(type(frame.get("id")) is int and frame["id"] == sequence
                        and isinstance(frame.get("result"), dict) and "error" not in frame,
                        f"desktop sidecar failed {method}")
                return frame["result"]
            raise ValueError(f"desktop sidecar stopped during {method}")

        async def timed_read(sequence: int, started: int, timer: int) -> tuple[dict, dict, int]:
            result = await rpc(sequence, "read_bytes", read_request)
            timing = {"started_at_unix_ms": started // 1_000_000, "finished_at_unix_ms": time.time_ns() // 1_000_000,
                      "elapsed_ms": (time.monotonic_ns() - timer + 999_999) // 1_000_000}
            # Read events precede the response. Each arm consumes only its trace segment.
            trace_end = os.fstat(trace.fileno()).st_size
            require(trace_end <= MAX_TRACE_BYTES, "desktop read trace exceeds the collection limit")
            return result, timing, trace_end

        try:
            async with asyncio.timeout(300):
                method, params = open_request
                repo_id = params["repo_id"]
                started, timer = time.time_ns(), time.monotonic_ns()
                opened = await rpc(1, method, params)
                require(opened.get("repo_id") == repo_id and opened.get("is_remote") is (method == "open_url")
                        and (method != "open_url" or opened.get("url") == params["url"]),
                        "desktop sidecar opened a different repository")
                samples = [await timed_read(2, started, timer)]
                if repeat:
                    samples.append(await timed_read(3, time.time_ns(), time.monotonic_ns()))
                require(await rpc(len(samples) + 2, "close", {"repo_id": repo_id}) == {"closed": repo_id},
                        "desktop sidecar did not close the captured repository")
                stdin.close()
                while line := await stdout.readline():
                    frame = decode(line)
                    require(isinstance(frame, dict) and frame.get("jsonrpc") == "2.0"
                            and "id" not in frame and isinstance(frame.get("method"), str),
                            "unexpected desktop RPC response after close")
                require(await process.wait() == 0, "desktop sidecar exited unsuccessfully")
        finally:
            if process.returncode is None:
                stdin.close()
                try:
                    await asyncio.wait_for(process.communicate(), timeout=10)
                except TimeoutError:
                    if process.returncode is None:
                        with suppress(ProcessLookupError):
                            process.kill()
                    await process.communicate()
        trace.seek(0)
        captured = []
        start = 0
        for result, timing, end in samples:
            captured.append((result, timing, trace.read(end - start).splitlines()))
            start = end
        return captured


def verified_payload(observed: dict, publication: dict) -> dict:
    require(type(observed.get("size")) is int and observed["size"] == GIT_PAYLOAD_SIZE and observed.get("truncated") is False
            and isinstance(observed.get("bytes_b64"), str), "desktop read did not return the complete published file")
    try:
        body = base64.b64decode(observed["bytes_b64"], validate=True)
    except ValueError:
        raise ValueError("desktop read returned invalid byte encoding") from None
    payload_sha256 = hashlib.sha256(body).hexdigest()
    require(len(body) == GIT_PAYLOAD_SIZE and payload_sha256 == publication["sha256"],
            "desktop read returned different bytes from the native publication")
    git_oid = hashlib.sha1(f"blob {len(body)}\0".encode() + body, usedforsecurity=False).digest()
    return {"sha256": payload_sha256, "bytes": len(body), "git_blob_oid_sha256": hashlib.sha256(git_oid).hexdigest()}


def measured_read(context: dict, timing: dict, lines: list[bytes], arm: str) -> dict:
    responses: dict = {}
    scope = context["scope"]
    counters = measure(lines, scope, edge_responses=responses, expected_git_blob=context["git_blob_oid_sha256"])
    return validate_desktop_git_read(context | timing | counters | {
        "arm": arm, "edge_responses": responses, "native_traffic": measure_native(lines, scope),
    }, scope, arm)


def fetch(args: argparse.Namespace) -> tuple[dict, dict | None]:
    require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id) and matches(RUN_NUMBER, args.run_attempt),
            "exact workflow run provenance is required")
    fixture = approved_fixture(args.fixture, args.configuration_sha256)
    scope = fixture["scope"]
    require(is_direct_desktop_git_scope(scope) and fixture["view_suffix"] == "",
            "desktop Git qualification uses a direct fixture on the repository prefix")
    repeat = args.warm_local_output is not None
    require(not repeat or args.arm == "edge", "desktop warm-local reads must repeat an edge primer")
    access = scope_parts(scope)[1]
    require(args.arm != "prior_export" or args.transition_object is not None,
            "desktop prior export needs the exact transition object")
    require(args.transition_object is None or (access == "public" and args.arm != "origin" and not repeat),
            "desktop transition requires a separate public read with an active receipt")
    provenance = {"scope": scope, "configuration_sha256": args.configuration_sha256, "source_sha": args.source_sha,
                  "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt}
    raw = read_git_record(args.receipt)
    publication = validate_git_publication(raw, provenance)
    require(publication["published"] and publication["visible_at_unix_ms"] <= time.time_ns() // 1_000_000,
            "desktop read requires a confirmed native publication")
    sidecar = args.sidecar.resolve(strict=True)
    platform = runner_platform()
    candidate = sidecar_candidate(sidecar, args.sidecar_sha256, args.source_sha, platform)
    binding, approval, transition = binding_for(args, fixture, args.transition_object)
    if args.arm == "prior_export":
        require(transition is not None and transition["sealed_at_unix_ms"] <= publication["visible_at_unix_ms"],
                "desktop prior public export must be sealed before the native publication")
    read_configuration(fixture, binding)
    identity = {"repo_id": "cdn-qualification"}
    with TemporaryDirectory(prefix="crab-cdn-desktop-", dir=args.scratch) as temporary:
        scratch = Path(temporary).resolve()
        cache = scratch / "cache"
        env = {key: value for key, value in os.environ.items() if not key.upper().startswith("GIT_")}
        env |= {"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_TERMINAL_PROMPT": "0",
                "CRAB_CACHE_DIR": str(cache), "CRAB_DESKTOP_LOG": (
                    f"{READ_TARGET}=debug,{TARGET}=debug,{NATIVE_TARGET}=debug,{GCS_VIEWER_LOG_TARGET}=trace"
                    + (f",{PUBLIC_TRANSITION_TARGET}=debug" if transition is not None else "")
                )}
        samples = asyncio.run(run_sidecar(
            [str(sidecar), "--log-json"], scratch, env,
            ("open_url", identity | {"url": desktop_url(fixture)}),
            identity | {"path": PAYLOAD, "rev": publication["oid"], "max_bytes": GIT_PAYLOAD_SIZE}, repeat=repeat,
        ))
    observed, timing, lines = samples[0]
    payload = verified_payload(observed, publication)
    require(publication["visible_at_unix_ms"] <= timing["started_at_unix_ms"],
            "desktop read started before the confirmed publication")
    reader = verified_target(lines, fixture, args.arm, approval)
    context = provenance | payload | {
        "schema": "crab.cdn-desktop-git-read/v1", "platform": platform,
        "publication_sha256": hashlib.sha256(raw).hexdigest(), "commit": publication["oid"], "candidate": candidate,
        "cache_root_sha256": hashlib.sha256(os.fsencode(cache)).hexdigest(), "reader": reader, "approval_sha256": approval,
    }
    if transition is not None:
        verify_transition_events(lines, scope, transition, git_blob=payload["git_blob_oid_sha256"])
        context["transition_key_sha256"] = transition["key_sha256"]
    primary = measured_read(context, timing, lines, args.arm)
    local = None
    if repeat:
        observed, local_timing, local_lines = samples[1]
        require(verified_payload(observed, publication) == payload
                and timing["finished_at_unix_ms"] <= local_timing["started_at_unix_ms"],
                "desktop repeat did not read the published blob after its primer")
        local = measured_read(context | {"primer_sha256": hashlib.sha256(json_record(primary).encode()).hexdigest()},
                              local_timing, local_lines, "warm_local")
    return primary, local


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("arm", choices=("edge", "origin", "prior_export"))
    parser.add_argument("--fixture", type=Path, required=True)
    parser.add_argument("--configuration-sha256", required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--sidecar", type=Path, required=True)
    parser.add_argument("--sidecar-sha256", required=True)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--run-attempt", required=True)
    parser.add_argument("--private-reader", type=Path)
    parser.add_argument("--export-approval", type=Path)
    parser.add_argument("--transition-object", help="exact repository-relative pack-layer key selected by the desktop read")
    parser.add_argument("--warm-local-output", type=Path, help="write the timed repeat in the same open sidecar repository")
    try:
        args = parser.parse_args()
        result, repeat = fetch(args)
        if repeat is not None:
            with args.warm_local_output.open("x", encoding="utf-8", newline="\n") as output:
                output.write(json_record(repeat))
    except (TimeoutError, subprocess.TimeoutExpired):
        print("error: desktop sidecar qualification timed out", file=sys.stderr)
        return 1
    except (ExportError, OSError, ValueError):
        print("error: desktop CDN read could not verify the fixture, sidecar or observations", file=sys.stderr)
        return 1
    # The repeat pins these exact bytes, including the LF used on Windows.
    sys.stdout.buffer.write(json_record(result).encode())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
