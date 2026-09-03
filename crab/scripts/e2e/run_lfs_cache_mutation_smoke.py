#!/usr/bin/env python3
"""Qualify LFS cache mutation during real Git checkout without a remote service."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import threading
import time


MARKER = b"crab-lfs-cache-integrity-probe\n"


def forward(cache_path, command):
    child = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE)

    def feed():
        try:
            while data := os.read(sys.stdin.fileno(), 65536):
                child.stdin.write(data)
                child.stdin.flush()
        except BrokenPipeError:
            pass
        finally:
            child.stdin.close()

    writer = threading.Thread(target=feed, daemon=True)
    writer.start()
    mutated = False
    try:
        while data := os.read(child.stdout.fileno(), 65536):
            if not mutated and MARKER in data:
                # The peer is already emitting the validated object. Its pipe
                # cannot hold the whole fixture, so later file reads remain.
                Path(cache_path).write_bytes(b"")
                mutated = True
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
        return child.wait(timeout=10)
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--crab-bin", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    args = parser.parse_args()
    binary = args.crab_bin.resolve()
    root = args.root.resolve() / args.run_id
    workspace = (Path.home() / "Workspace").resolve()
    if not root.is_relative_to(workspace) or root == workspace:
        raise RuntimeError("qualification root must be beneath the workspace volume")
    root.mkdir()
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    report = {"schema": "crab.lfs-cache-mutation", "status": "running",
              "binary_sha256": binary_hash,
              "driver_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "valid_for_comparison": False, "commands": [], "checks": []}
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("GIT_"):
            env.pop(key)
    env.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_TERMINAL_PROMPT": "0", "CRAB_CACHE_DIR": str(root / "cache")})

    def run(name, command, cwd):
        started = time.monotonic()
        process = subprocess.Popen(command, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=os.name != "nt")
        try:
            stdout, stderr = process.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            if os.name == "nt":
                subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            else:
                os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate()
            process.returncode = -124
        result = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
        index = len(report["commands"])
        (root / f"{index:03d}.stdout").write_bytes(result.stdout)
        (root / f"{index:03d}.stderr").write_bytes(result.stderr)
        report["commands"].append({"name": name, "args": [str(arg) for arg in command],
                                    "exit_code": result.returncode,
                                    "duration_ms": int((time.monotonic() - started) * 1000)})
        return result

    def check(name, passed, detail=None):
        report["checks"].append({"name": name, "ok": passed, "detail": detail})

    try:
        version = run("candidate version", [binary, "version", "--json"], root)
        report["binary_metadata"] = json.loads(version.stdout)["data"]
        git_version = run("Git version", ["git", "--version"], root)
        if git_version.returncode:
            raise RuntimeError("Git version probe failed")
        report["git_version"] = git_version.stdout.decode().strip()
        content = (MARKER * (4 * 1024 * 1024 // len(MARKER) + 1))[:4 * 1024 * 1024]
        oid = hashlib.sha256(content).hexdigest()
        report["fixture"] = {"size": len(content), "sha256": oid}
        pointer = f"version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {len(content)}\n"
        for name, suffix in (("native", ["filter-process"]), ("lfs", ["lfs", "filter-process"]),
                             ("standalone", ["lfs", "smudge"])):
            repo = root / name
            repo.mkdir()
            git = ["git", "-C", str(repo)]
            for options in (["init", "--quiet"], ["config", "user.name", "Qualification"],
                            ["config", "user.email", "qualification@example.invalid"],
                            ["config", "filter.probe.required", "true"],
                            ["config", "filter.probe.clean", "cat"]):
                if run(name + " setup", git + options, root).returncode:
                    raise RuntimeError("Git fixture setup failed")
            (repo / ".gitattributes").write_text("asset.bin filter=probe -text\n")
            asset = repo / "asset.bin"
            asset.write_text(pointer)
            for options in (["add", ".gitattributes", "asset.bin"], ["commit", "-qm", "pointer fixture"]):
                if run(name + " commit fixture", git + options, root).returncode:
                    raise RuntimeError("Git fixture commit failed")
            cache = repo / ".git/lfs/objects" / oid[:2] / oid[2:4] / oid
            cache.parent.mkdir(parents=True)
            cache.write_bytes(content)
            key = "filter.probe.smudge" if name == "standalone" else "filter.probe.process"
            command = [str(binary), *suffix]
            if run(name + " install filter", git + ["config", key, shlex.join(command)], root).returncode:
                raise RuntimeError("filter setup failed")
            # checkout-index can skip stat-clean paths even with --force.
            # Change the worktree size so the real smudge path must run.
            asset.write_bytes(b"replace this worktree file")
            healthy = run(name + " healthy checkout", git + ["checkout-index", "--force", "--all"], root)
            check(name + " healthy bytes", healthy.returncode == 0 and asset.read_bytes() == content)
            asset.write_bytes(b"replace this worktree file")
            command = [sys.executable, str(Path(__file__).resolve()), "--forward", str(cache), *command]
            if run(name + " install fault peer", git + ["config", key, shlex.join(command)], root).returncode:
                raise RuntimeError("fault peer setup failed")
            failed = run(name + " checkout during cache truncation", git + ["checkout-index", "--force", "--all"], root)
            check(name + " rejects truncated cache", cache.stat().st_size == 0 and failed.returncode > 0,
                  {"exit_code": failed.returncode, "worktree_bytes": asset.stat().st_size if asset.exists() else None})
        check("candidate unchanged", hashlib.sha256(binary.read_bytes()).hexdigest() == binary_hash)
        report["status"] = "passed" if all(item["ok"] for item in report["checks"]) else "failed"
    except Exception as error:
        report["status"] = "failed"
        report["error"] = str(error)
    finally:
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "error": report.get("error"), "report": str(root / "report.json")}))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    if sys.argv[1:2] == ["--forward"]:
        raise SystemExit(forward(sys.argv[2], sys.argv[3:]))
    raise SystemExit(main())
