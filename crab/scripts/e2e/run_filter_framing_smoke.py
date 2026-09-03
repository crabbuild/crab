#!/usr/bin/env python3
"""Qualify real filter CLI framing and terminal failures in an isolated Git repo."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time


def packet(body):
    return f"{len(body) + 4:04x}".encode() + body


def text(value):
    return packet(value.encode() + b"\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crab-bin", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    args = parser.parse_args()
    binary = args.crab_bin.resolve()
    root = (args.root / args.run_id).resolve()
    workspace = (Path.home() / "Workspace").resolve()
    if not root.is_relative_to(workspace) or root == workspace:
        raise RuntimeError("qualification root must be beneath the workspace volume")
    root.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_TERMINAL_PROMPT": "0", "CRAB_CACHE_DIR": str(root / "cache")})
    report = {"schema": "crab.filter-framing", "status": "running",
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "driver_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "valid_for_comparison": False, "checks": []}
    try:
        subprocess.run(["git", "init", "--quiet", str(root)], env=env, check=True, timeout=10)
        version = subprocess.run([binary, "version", "--json"], env=env, cwd=root,
                                 capture_output=True, check=True, timeout=10)
        report["binary_metadata"] = json.loads(version.stdout)["data"]
        version = subprocess.run(["git", "--version"], capture_output=True, check=True, timeout=10)
        report["git_version"] = version.stdout.decode().strip()
        flush = b"0000"
        caps = b"".join(text("capability=" + cap) for cap in ("clean", "smudge", "delay")) + flush
        welcome = text("git-filter-client") + text("version=2") + flush
        handshake = welcome + caps
        server_welcome = text("git-filter-server") + text("version=2") + flush
        response = server_welcome + caps
        smudge = text("command=smudge") + text("pathname=plain.txt") + flush
        query = text("command=list_available_blobs") + flush
        body = b"exact ordinary content"
        success = text("status=success") + flush + packet(body) + flush + flush
        empty_success = text("status=success") + flush + flush + flush
        cases = [
            ("clean EOF", handshake, response, False, False),
            ("bodyless query between smudges", handshake + smudge + packet(body) + flush + query + smudge + flush,
             response + success + flush + text("status=success") + flush + empty_success, False, False),
            ("unknown command with open input", handshake + text("command=unsupported") + flush, response, True, True),
            ("unknown command before valid-looking payload", handshake + text("command=unsupported") + flush + smudge + packet(body) + flush,
             response, True, False),
            ("wrong command key", handshake + text("operation=smudge"), response, True, False),
            ("partial length", handshake + b"001", response, True, False),
            ("partial text", handshake + b"0010command=", response, True, False),
            ("unexpected flush", handshake + flush, response, True, False),
            ("missing list flush", handshake + text("command=list_available_blobs"), response, True, False),
            ("missing pathname", handshake + text("command=smudge") + flush + flush, response, True, False),
            ("header delimiter", handshake + smudge[:-4] + b"0001" + flush, response, True, False),
            ("header response end", handshake + smudge[:-4] + b"0002" + flush, response, True, False),
            ("content delimiter", handshake + smudge + b"0001", response, True, False),
            ("content response end", handshake + smudge + b"0002", response, True, False),
            ("missing capability flush", handshake[:-4], server_welcome, True, False),
        ]
        for mode, suffix in (("native", ["filter-process"]), ("lfs", ["lfs", "filter-process"])):
            for name, payload, expected, must_fail, keep_open in cases:
                index = len(report["checks"])
                started = time.monotonic()
                with (root / f"{index:03d}.stdout").open("wb") as output, (root / f"{index:03d}.stderr").open("wb") as errors:
                    process = subprocess.Popen([binary, *suffix], cwd=root, env=env, stdin=subprocess.PIPE,
                                               stdout=output, stderr=errors, start_new_session=os.name != "nt")
                    try:
                        process.stdin.write(payload)
                        process.stdin.flush()
                        if not keep_open:
                            process.stdin.close()
                        exit_code = process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        if os.name == "nt":
                            subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                        else:
                            os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
                        exit_code = -124
                    finally:
                        if process.poll() is None:
                            process.kill()
                            process.wait()
                        try:
                            process.stdin.close()
                        except BrokenPipeError:
                            pass
                actual = (root / f"{index:03d}.stdout").read_bytes()
                report["checks"].append({"name": mode + ": " + name,
                                         "ok": actual == expected and (exit_code > 0 if must_fail else exit_code == 0),
                                         "exit_code": exit_code, "exact_response": actual == expected,
                                         "duration_ms": int((time.monotonic() - started) * 1000)})
        report["checks"].append({"name": "candidate unchanged",
                                 "ok": hashlib.sha256(binary.read_bytes()).hexdigest() == report["binary_sha256"]})
        report["status"] = "passed" if all(check["ok"] for check in report["checks"]) else "failed"
    except Exception as error:
        report["status"] = "failed"
        report["error"] = str(error)
    finally:
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "error": report.get("error"), "report": str(root / "report.json")}))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
