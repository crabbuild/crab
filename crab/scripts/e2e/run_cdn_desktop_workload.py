#!/usr/bin/env python3
"""Read a published CDN fixture through the ordinary desktop sidecar RPC stream."""

from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import json
import os
import sys
import time
import tomllib
from contextlib import suppress
from pathlib import Path
from tempfile import TemporaryDirectory, TemporaryFile

from collect_cdn_product_metrics import NATIVE_TARGET, TARGET, measure, measure_native
from run_cdn_git_workload import GCS_VIEWER_LOG_TARGET, PAYLOAD, runner_platform
from run_cdn_sdk_workload import READ_TARGET, binding_for, json_record, verified_target
from run_cdn_transport_checks import approved_fixture

from cdn_qualification import (
    COMMIT, GIT_PAYLOAD_SIZE, RUN_NUMBER, SHA256, decode, is_direct_desktop_git_scope, matches,
    read_git_record, require, validate_desktop_git_read, validate_git_publication,
)
from public_inventory import ExportError


MAX_TRACE_BYTES = 8 * 1024 * 1024
RPC_FRAME_LIMIT = 512 * 1024


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
                      read_request: dict) -> tuple[dict, dict, list[bytes]]:
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

        try:
            async with asyncio.timeout(300):
                method, params = open_request
                repo_id = params["repo_id"]
                started, timer = time.time_ns(), time.monotonic_ns()
                opened = await rpc(1, method, params)
                require(opened.get("repo_id") == repo_id and opened.get("is_remote") is (method == "open_url")
                        and (method != "open_url" or opened.get("url") == params["url"]),
                        "desktop sidecar opened a different repository")
                result = await rpc(2, "read_bytes", read_request)
                timing = {"started_at_unix_ms": started // 1_000_000, "finished_at_unix_ms": time.time_ns() // 1_000_000,
                          "elapsed_ms": (time.monotonic_ns() - timer + 999_999) // 1_000_000}
                # The sidecar emits read events before its response. Snapshot the trace length
                # after that response so close and shutdown cannot add traffic to the sample.
                trace_end = os.fstat(trace.fileno()).st_size
                require(trace_end <= MAX_TRACE_BYTES, "desktop read trace exceeds the collection limit")
                require(await rpc(3, "close", {"repo_id": repo_id}) == {"closed": repo_id},
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
        return result, timing, trace.read(trace_end).splitlines()


def fetch(args: argparse.Namespace) -> dict:
    require(matches(COMMIT, args.source_sha) and matches(RUN_NUMBER, args.run_id) and matches(RUN_NUMBER, args.run_attempt),
            "exact workflow run provenance is required")
    fixture = approved_fixture(args.fixture, args.configuration_sha256)
    scope = fixture["scope"]
    require(is_direct_desktop_git_scope(scope) and fixture["view_suffix"] == "",
            "desktop Git qualification uses a direct fixture on the repository prefix")
    provenance = {"scope": scope, "configuration_sha256": args.configuration_sha256, "source_sha": args.source_sha,
                  "workflow_run_id": args.run_id, "workflow_run_attempt": args.run_attempt}
    raw = read_git_record(args.receipt)
    publication = validate_git_publication(raw, provenance)
    require(publication["published"] and publication["visible_at_unix_ms"] <= time.time_ns() // 1_000_000,
            "desktop read requires a confirmed native publication")
    sidecar = args.sidecar.resolve(strict=True)
    with sidecar.open("rb") as executable:
        binary_sha256 = hashlib.file_digest(executable, "sha256").hexdigest()
    require(binary_sha256 == args.sidecar_sha256, "desktop executable differs from the preregistered build digest")
    binding, approval, _ = binding_for(args, fixture)
    read_configuration(fixture, binding)
    identity = {"repo_id": "cdn-qualification"}
    with TemporaryDirectory(prefix="crab-cdn-desktop-", dir=args.scratch) as temporary:
        scratch = Path(temporary).resolve()
        cache = scratch / "cache"
        env = {key: value for key, value in os.environ.items() if not key.upper().startswith("GIT_")}
        env |= {"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_TERMINAL_PROMPT": "0",
                "CRAB_CACHE_DIR": str(cache), "CRAB_DESKTOP_LOG": (
                    f"{READ_TARGET}=debug,{TARGET}=debug,{NATIVE_TARGET}=debug,{GCS_VIEWER_LOG_TARGET}=trace"
                )}
        observed, timing, lines = asyncio.run(run_sidecar(
            [str(sidecar), "--log-json"], scratch, env,
            ("open_url", identity | {"url": desktop_url(fixture)}),
            identity | {"path": PAYLOAD, "rev": publication["oid"], "max_bytes": GIT_PAYLOAD_SIZE},
        ))
    require(type(observed.get("size")) is int and observed["size"] == GIT_PAYLOAD_SIZE and observed.get("truncated") is False
            and isinstance(observed.get("bytes_b64"), str), "desktop read did not return the complete published file")
    require(publication["visible_at_unix_ms"] <= timing["started_at_unix_ms"],
            "desktop read started before the confirmed publication")
    try:
        body = base64.b64decode(observed["bytes_b64"], validate=True)
    except ValueError:
        raise ValueError("desktop read returned invalid byte encoding") from None
    payload_sha256 = hashlib.sha256(body).hexdigest()
    require(len(body) == GIT_PAYLOAD_SIZE and payload_sha256 == publication["sha256"],
            "desktop read returned different bytes from the native publication")
    git_oid = hashlib.sha1(f"blob {len(body)}\0".encode() + body, usedforsecurity=False).digest()
    blob_sha256 = hashlib.sha256(git_oid).hexdigest()
    reader = verified_target(lines, fixture, args.arm, approval)
    responses: dict = {}
    counters = measure(lines, scope, edge_responses=responses, expected_git_blob=blob_sha256)
    client = provenance | timing | counters | {
        "schema": "crab.cdn-desktop-git-read/v1", "arm": args.arm, "platform": runner_platform(),
        "publication_sha256": hashlib.sha256(raw).hexdigest(), "commit": publication["oid"], "sha256": payload_sha256,
        "bytes": len(body), "git_blob_oid_sha256": blob_sha256, "binary_sha256": binary_sha256,
        "cache_root_sha256": hashlib.sha256(os.fsencode(cache)).hexdigest(), "reader": reader, "approval_sha256": approval,
        "edge_responses": responses, "native_traffic": measure_native(lines, scope),
    }
    return validate_desktop_git_read(client, scope, args.arm)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("arm", choices=("edge", "origin"))
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
    try:
        result = fetch(parser.parse_args())
    except TimeoutError:
        print("error: desktop sidecar read timed out", file=sys.stderr)
        return 1
    except (ExportError, OSError, ValueError):
        print("error: desktop CDN read could not verify the fixture, sidecar or observations", file=sys.stderr)
        return 1
    print(json_record(result), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
