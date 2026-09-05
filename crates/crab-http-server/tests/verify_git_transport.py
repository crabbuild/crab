"""Qualify large Git HTTP requests against an independent native-Git source."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
from urllib.parse import urlsplit


def git(args, *, cwd=None, data=None):
    environment = dict(os.environ, GIT_TERMINAL_PROMPT="0")
    result = subprocess.run(
        ["git", "-c", "protocol.version=2", *args],
        cwd=cwd,
        input=data,
        env=environment,
        capture_output=True,
        timeout=300,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace"))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", required=True, help="HTTP clone URL, without credentials")
    parser.add_argument("--source", required=True, type=Path, help="Read-only native Git oracle")
    parser.add_argument("--revision", required=True, help="Full uploaded commit OID")
    parser.add_argument("--workdir", required=True, type=Path, help="Existing workspace-volume directory")
    args = parser.parse_args()
    parsed = urlsplit(args.url)
    if parsed.scheme not in ("http", "https") or parsed.username or parsed.password:
        parser.error("Use an HTTP(S) URL without credentials; configure a credential helper or GIT_ASKPASS")
    if not re.fullmatch(r"[0-9a-f]{40}", args.revision):
        parser.error("--revision must be a full SHA-1 commit OID")
    if not args.workdir.is_dir():
        parser.error("--workdir must already exist on the workspace volume")

    entries = git(["ls-tree", "-rlz", args.revision], cwd=args.source).stdout.split(b"\0")
    wanted = dict()
    for entry in entries:
        if not entry:
            continue
        fields = entry.split(b"\t", 1)[0].split()
        if fields[1] == b"blob" and int(fields[3]) < 2048:
            wanted[fields[2].decode()] = None
        if len(wanted) == 1600:
            break
    if len(wanted) < 1600:
        parser.error("The fixture needs 1,600 distinct blobs smaller than 2 KiB (for example Kubernetes)")
    batch = ("\n".join(wanted) + "\n").encode()
    expected = git(["cat-file", "--batch"], cwd=args.source, data=batch).stdout
    report = {"revision": args.revision, "verified_blobs": len(wanted), "transfers": []}
    for mode, buffer_size in [("buffered", 4 * 1024 * 1024), ("chunked", 128 * 1024)]:
        client = Path(tempfile.mkdtemp(prefix=f"git-http-{mode}-", dir=args.workdir))
        git(["init", "--bare", str(client)])
        started = time.monotonic()
        result = git(
            [
                "-c", f"http.postBuffer={buffer_size}", "fetch", "-vvv", "--filter=blob:none",
                "--no-tags", "--no-write-fetch-head", args.url, *wanted,
            ],
            cwd=client,
        )
        elapsed = round((time.monotonic() - started) * 1000, 3)
        observed = git(["cat-file", "--batch"], cwd=client, data=batch).stdout
        if observed != expected:
            raise RuntimeError(f"{mode} fetch did not reproduce the oracle's exact blob bytes")
        transfer = {
            "mode": mode,
            "fetch_ms": elapsed,
            "client": str(client),
            "http_requests": [line for line in result.stderr.decode(errors="replace").splitlines()
                              if line.startswith("POST git-upload-pack")],
        }
        report["transfers"].append(transfer)
        print(json.dumps(transfer), flush=True)
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
