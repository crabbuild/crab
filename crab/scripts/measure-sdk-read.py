#!/usr/bin/env python3
"""Measure one isolated Linux SDK read and verify its complete result."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import resource
import subprocess
import sys
import time
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("bucket")
    parser.add_argument("repository")
    parser.add_argument("branch")
    parser.add_argument("path")
    parser.add_argument("cache", type=Path)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--bytes", type=int, required=True)
    parser.add_argument("--blake3", required=True)
    parser.add_argument("--cache-bytes", type=int)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("this runner uses Linux ru_maxrss units")
    if args.cache_bytes is not None and args.cache_bytes <= 0:
        parser.error("cache bytes must be positive")
    executable = args.executable.resolve(strict=True)
    cache = args.cache.resolve(strict=True)
    if not cache.is_dir():
        parser.error("cache must be an existing directory")
    with executable.open("rb") as binary:
        executable_sha256 = hashlib.file_digest(binary, "sha256").hexdigest()
    command = [str(executable), args.bucket, args.repository, args.branch,
               args.path, "hydrated", str(cache)]
    if args.cache_bytes is not None:
        command.append(str(args.cache_bytes))
    started = time.monotonic()
    terminal_state = "exited"
    launch_error = None
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=180)
        exit_code, stdout, stderr = result.returncode, result.stdout, result.stderr
    except subprocess.TimeoutExpired as error:
        # run() kills and waits for its child before raising. Preserve the failed
        # measurement as evidence instead of losing it to an unstructured traceback.
        terminal_state = "timed_out"
        exit_code = None
        stdout = error.stdout.decode(errors="replace") if error.stdout else ""
        stderr = error.stderr.decode(errors="replace") if error.stderr else ""
    except OSError as error:
        terminal_state = "spawn_failed"
        exit_code, stdout, stderr = None, "", ""
        launch_error = {"errno": error.errno, "message": str(error)}
    elapsed = time.monotonic() - started
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    expected = f"commit={args.commit} bytes={args.bytes} blake3={args.blake3}\n"
    verified = exit_code == 0 and stdout == expected
    # This fresh Python process launches exactly one child. Linux reports its
    # peak RSS in KiB; no prior child can contaminate this process's high-water mark.
    report = {
        "executable_sha256": executable_sha256,
        "platform": platform.platform(),
        "command": command,
        "expected": {
            "commit": args.commit,
            "bytes": args.bytes,
            "blake3": args.blake3,
        },
        "elapsed_seconds": elapsed,
        "peak_rss_bytes": usage.ru_maxrss * 1024,
        "user_seconds": usage.ru_utime,
        "system_seconds": usage.ru_stime,
        "terminal_state": terminal_state,
        "launch_error": launch_error,
        "exit_code": exit_code,
        "verified": verified,
        "stdout": stdout,
        "stderr": stderr,
    }
    print(json.dumps(report, indent=2))
    if not verified:
        sys.exit(1)


if __name__ == "__main__":
    main()
