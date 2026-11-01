#!/usr/bin/env python3
"""Verify live Capsule push progress and its stdout/stderr contracts on RustFS."""

from __future__ import annotations

import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import struct
import subprocess
import termios
import time
from datetime import datetime, timezone


def terminal_push(command: list[str], cwd: Path, env: dict[str, str], columns: int) -> tuple[int, str]:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, columns, 0, 0))
    process = subprocess.Popen(
        command, cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave,
        start_new_session=True,
    )
    os.close(slave)
    output = bytearray()
    deadline = time.monotonic() + 180
    try:
        while True:
            if time.monotonic() > deadline:
                raise TimeoutError("push exceeded 180 seconds")
            if select.select([master], [], [], 0.2)[0]:
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not chunk:
                    break
                output.extend(chunk)
            elif process.poll() is not None:
                break
        return process.wait(), output.decode(errors="replace")
    finally:
        os.close(master)
        if process.poll() is None:
            process.kill()
            process.wait()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True, help="Scratch parent on the Workspace volume")
    parser.add_argument("--crab-bin", type=Path, default=Path(shutil.which("crab") or "crab"))
    parser.add_argument(
        "--bucket", required=True,
        help="Dedicated test bucket; repository prefixes share its global chunk-hint index",
    )
    parser.add_argument("--endpoint-url", default="http://127.0.0.1:9000")
    args = parser.parse_args()
    run_id = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S-%f")
    root = args.root / f"push-progress-{run_id}"
    source = root / "source"
    source.mkdir(parents=True)
    executable = args.crab_bin.resolve()
    binaries = root / "bin"
    binaries.mkdir()
    for name in ("crab", "git-remote-crab"):
        (binaries / name).symlink_to(executable)
    crab = str(binaries / "crab")
    env = dict(os.environ)
    env.update({
        "PATH": str(binaries) + os.pathsep + env.get("PATH", ""),
        "AWS_ENDPOINT_URL": args.endpoint_url,
        "AWS_ENDPOINT_URL_S3": args.endpoint_url,
        "AWS_ALLOW_HTTP": "true",
        "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
        "AWS_EC2_METADATA_DISABLED": "true",
        "TERM": "xterm-256color",
    })
    env.pop("CRAB_PROGRESS_FORMAT", None)

    def run(label: str, command: list[str], cwd: Path = source) -> str:
        result = subprocess.run(command, cwd=cwd, env=env, capture_output=True, text=True, timeout=180)
        (root / f"{label}.log").write_text(result.stdout + result.stderr)
        if result.returncode:
            raise RuntimeError(f"{label} failed; inspect {root / (label + '.log')}")
        return result.stdout

    remote = f"crab://{args.bucket}/progress-smoke/{run_id}"
    run("init", ["git", "init", "-b", "main"])
    run("configure", [crab, "configure", remote, "--provider", "s3", "--track", "*.bin"])
    cases = (
        ("concurrent-tty", [crab, "push", "--upload-concurrency", "4", "origin", "main"], 120, {}),
        ("native-tty", [crab, "push", "origin", "main"], 100, {}),
        ("narrow-tty", [crab, "push", "--verbose", "--no-color", "origin", "main"], 40, {}),
        ("dumb-tty", [crab, "push", "origin", "main"], 80, {"TERM": "dumb"}),
        ("native-json", [crab, "push", "--json", "origin", "main"], 0, {}),
        ("native-jsonl", [crab, "push", "--jsonl", "origin", "main"], 0, {}),
        ("helper-tty", ["git", "push", "--progress", "origin", "main"], 100, {}),
        ("helper-jsonl", ["git", "push", "--progress", "origin", "main"], 0, {"CRAB_PROGRESS_FORMAT": "jsonl"}),
        ("helper-quiet", ["git", "push", "--quiet", "--no-progress", "origin", "main"], 80, {}),
    )
    report = []
    for name, command, columns, overrides in cases:
        with (source / "sample.bin").open("ab") as file:
            for _ in range(20 if name == "concurrent-tty" else 1):
                file.write(os.urandom(8 * 1024 * 1024))
        run(f"{name}-add", [crab, "add", "sample.bin"])
        run(f"{name}-commit", ["git", "-c", "user.name=Crab Progress Smoke", "-c", "user.email=progress@example.invalid", "commit", "-m", name])
        case_env = env | overrides
        if name == "concurrent-tty":
            case_env.pop("NO_COLOR", None)
        started = time.monotonic()
        if columns:
            code, output = terminal_push(command, source, case_env, columns)
            (root / f"{name}.tty").write_text(output)
            assert code == 0, f"{name}: push failed"
            if name in ("dumb-tty", "helper-quiet"):
                assert "\x1b" not in output, f"{name}: terminal control sequences in plain output"
            else:
                assert "\x1b[2K" in output, f"{name}: no live terminal frames"
                assert "Push complete" in output, f"{name}: display did not finish"
            if name != "helper-quiet":
                assert any(label in output for label in ("Preparing", "Uploading", "Publishing", "Finalizing")), name
            if "--no-color" in command:
                assert not re.search(r"\x1b\[[0-9;]*m", output), "--no-color emitted color"
            if name == "concurrent-tty":
                assert "\x1b[1;36m" in output and "█" in output, "missing rich colored bars"
                plain = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", output)
                frames = re.split(r"\r(?=[|/\\-] (?:Preparing|Uploading|Writing|Publishing|Finalizing))", plain)
                peak = max(len(set(re.findall(r"xorb ([0-9a-f]{10})", frame))) for frame in frames)
                assert peak >= 2, f"only {peak} concurrent upload rows observed"
                print(f"PASS: {peak} concurrent xorb rows in a live frame", flush=True)
            event_types = []
        else:
            result = subprocess.run(command, cwd=source, env=case_env, capture_output=True, text=True, timeout=180)
            (root / f"{name}.stdout").write_text(result.stdout)
            (root / f"{name}.stderr").write_text(result.stderr)
            assert result.returncode == 0, f"{name}: push failed"
            stream = result.stderr if name.startswith("helper") else result.stdout
            if name == "native-json":
                envelope = json.loads(stream)
                assert envelope["schema"] == "push" and envelope["data"]["refs_pushed"] == 1, envelope
                event_types = []
            else:
                events = [json.loads(line) for line in stream.splitlines() if line.startswith("{")]
                event_types = [event["type"] for event in events]
                assert "progress" in event_types, f"{name}: no progress events"
                assert event_types.count("snapshot") == 1, f"{name}: missing final transfer snapshot"
                transferred = max(event["data"].get("bytes", 0) for event in events if event["type"] in ("progress", "snapshot"))
                assert transferred >= 8 * 1024 * 1024, f"{name}: missing verified data bytes"
                assert event_types[-1] == "result" and event_types.count("result") == 1, event_types
            assert "\x1b" not in stream, f"{name}: ANSI in JSONL stream"
            if name.startswith("helper"):
                assert not result.stdout.strip(), "Git helper diagnostics leaked to stdout"
        report.append({"case": name, "seconds": round(time.monotonic() - started, 2), "event_types": event_types})
        print(f"PASS: {name}", flush=True)

    run("stale-branch", ["git", "branch", "stale", "HEAD~1"])
    code, output = terminal_push([crab, "push", "origin", "stale:main"], source, env, 80)
    (root / "rejected-push.tty").write_text(output)
    assert code != 0 and "Push stopped" in output and "100%" not in output, "rejected push appeared successful"
    print("PASS: rejected push stays incomplete", flush=True)

    expected = hashlib.sha256((source / "sample.bin").read_bytes()).digest()
    clone = root / "clone"
    run("clone", [crab, "clone", "--lazy", remote, str(clone)], root)
    run("hydrate", [crab, "hydrate", "--all"], clone)
    assert hashlib.sha256((clone / "sample.bin").read_bytes()).digest() == expected
    run("fsck", ["git", "fsck", "--strict"], clone)
    (root / "report.json").write_text(json.dumps({"status": "passed", "remote": remote, "cases": report}, indent=2) + "\n")
    print(f"PASS: fresh clone, hydration, byte identity, strict fsck; report: {root / 'report.json'}")


if __name__ == "__main__":
    main()
