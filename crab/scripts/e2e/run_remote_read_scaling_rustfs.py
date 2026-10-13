#!/usr/bin/env python3
"""Qualify fixed read workloads while increasing unrelated Git history on S3.

Uses the existing smoke runner, HTTP meter and native Git oracle. Source fixtures
and receipts belong on the mounted workspace volume. Every workload runs in a
fresh process; its cold/warm query and opening measurements are reported apart.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

from run_add_commit_push_rustfs_smoke import AddCommitPushSmoke, REPO_ROOT, sha256_file
from run_concurrent_push_smoke import RequestCountingProxy
from verify_remote_browse import verify

WORKLOADS = ("resolve", "commit", "tree", "blob", "object-point", "object-batch",
             "history", "path-history", "attribution", "blame")


def fixture_import(commits: int, changes: int = 20) -> bytes:
    if commits <= changes:
        raise ValueError("history must exceed the fixed number of file changes")
    stream = bytearray()
    positions = {(number * (commits - 1)) // changes: number for number in range(1, changes + 1)}
    lines = [f"original line {number}\n" for number in range(50)]

    def file(path: str, body: bytes) -> None:
        stream.extend(f"M 100644 inline {path}\ndata {len(body)}\n".encode())
        stream.extend(body + b"\n")

    for revision in range(commits):
        stream.extend(f"commit refs/heads/main\ncommitter Qualification <qualification@example.invalid> {1700000000 + revision} +0000\ndata 0\n\n".encode())
        if revision == 0:
            for number in range(100):
                file(f"file-{number:03}.txt", f"fixed file {number}\n".encode())
        change = positions.get(revision)
        if revision == 0 or change is not None:
            if change is not None:
                lines[change - 1] = f"edited line {change}\n"
            file("target.txt", "".join(lines).encode())
        file("unrelated.txt", f"unrelated revision {revision}\n".encode())
        stream.extend(b"\n")
    stream.extend(b"done\n")
    return bytes(stream)


class ReadScalingRunner(AddCommitPushSmoke):
    def measured(self, name: str, args: list[str], cwd: Path):
        timed = ["/usr/bin/time", "-l" if sys.platform == "darwin" else "-v", *args]
        result = self.run_cmd(name, timed, cwd, timeout=self.args.timeout)
        stderr = Path(result.stderr_log).read_text()
        if sys.platform == "darwin":
            rss = re.search(r"(\d+)\s+maximum resident set size", stderr)
            cpu = re.search(r"([\d.]+) user\s+([\d.]+) sys", stderr)
        else:
            rss = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", stderr)
            user = re.search(r"User time \(seconds\):\s*([\d.]+)", stderr)
            system = re.search(r"System time \(seconds\):\s*([\d.]+)", stderr)
            cpu = None
        resources = {"peak_rss_bytes": int(rss[1]) * (1 if sys.platform == "darwin" else 1024) if rss else None}
        if sys.platform == "darwin":
            resources["cpu_seconds"] = float(cpu[1]) + float(cpu[2]) if cpu else None
        else:
            resources["cpu_seconds"] = float(user[1]) + float(system[1]) if user and system else None
        self.check(name + "-resource-accounting", all(value is not None for value in resources.values()))
        return result, resources

    def qualify(self) -> None:
        self.preflight()
        self.report.artifacts["harness_sha256"] = sha256_file(Path(__file__))
        self.report.artifacts["qualifier_sha256"] = sha256_file(self.args.qualifier)
        source_root = self.artifacts / "qualified-source"
        source_root.mkdir()
        inputs = (
            "Cargo.lock", "crates/crab-remote-git/Cargo.toml",
            "crates/crab-remote-git/examples/qualify_remote.rs",
            "crates/crab-remote-git/examples/support/mod.rs",
            "crates/crab-metadata/src/path_state/codec.rs",
            "crates/crab-metadata/src/split_commit_graph.rs",
            "crab/scripts/e2e/run_remote_read_scaling_rustfs.py",
            "crab/scripts/e2e/run_concurrent_push_smoke.py",
            "crab/scripts/e2e/verify_remote_browse.py",
        )
        hashes = {}
        for relative in inputs:
            target = source_root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((REPO_ROOT / relative).read_bytes())
            hashes[relative] = sha256_file(target)
        manifest = source_root / "sha256.json"
        manifest.write_text(json.dumps(hashes, indent=2) + "\n")
        self.report.artifacts["qualified_source"] = str(manifest)
        self.report.artifacts["source_patch_sha256"] = sha256_file(Path(self.run_git(REPO_ROOT, ["diff", "HEAD", "--binary"]).stdout_log))
        results = []
        for commits in self.args.history:
            repo, remote, prefix = self.prepare_git_repo(f"history-{commits}")
            source = fixture_import(commits)
            import_file = self.artifacts / f"history-{commits}.fi"
            import_file.write_bytes(source)
            imported = subprocess.run(["git", "fast-import", "--quiet"], input=source, cwd=repo,
                                      env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=self.args.timeout)
            record = self.record_command("fixture import", ["git", "fast-import", "--quiet"], repo,
                                         imported.returncode, 0, imported.stdout.decode(), imported.stderr.decode())
            self.check(f"history-{commits}-import", record.exit_code == 0)
            self.check(f"history-{commits}-fixed-file-changes", int(self.read_stdout(
                self.run_git(repo, ["rev-list", "--count", "HEAD", "--", "target.txt"]))) == 21)
            self.run_git(repo, ["fsck", "--full", "--strict"])
            self.run_crab(repo, ["push", "origin", "HEAD:refs/heads/main"], name=f"history-{commits} publish")
            proxy = RequestCountingProxy(self.args.endpoint_url, self.args.bucket + "/" + prefix)
            proxy.start()
            old_endpoint = {key:self.env.get(key) for key in ("AWS_ENDPOINT_URL", "AWS_ENDPOINT_URL_S3")}
            self.env.update(AWS_ENDPOINT_URL=proxy.url, AWS_ENDPOINT_URL_S3=proxy.url)
            try:
                before = proxy.snapshot()
                maintenance, resources = self.measured(f"history-{commits} build-indexes",
                    [str(self.args.qualifier), self.args.bucket, prefix, "target.txt", "build-indexes"], repo)
                results.append({"commits":commits, "operation":"build-indexes", "resources":resources,
                                "transport":RequestCountingProxy.delta(before, proxy.snapshot()), "evidence":maintenance.stdout_log})
                for name in WORKLOADS:
                    for repeat in range(self.args.repeats):
                        before = proxy.snapshot()
                        record, resources = self.measured(f"history-{commits} {name} repeat-{repeat}",
                            [str(self.args.qualifier), self.args.bucket, prefix, "target.txt", name], repo)
                        proof = verify(repo, Path(record.stdout_log), "HEAD")
                        self.check(f"history-{commits}-{name}-{repeat}-git-oracle", proof["status"] == "passed")
                        results.append({"commits":commits, "fixed_file_changes":20, "fixed_directory_entries":100,
                                        "operation":name, "repeat":repeat, "resources":resources, "proof":proof,
                                        "transport":RequestCountingProxy.delta(before, proxy.snapshot()), "evidence":record.stdout_log})
                        self.write_results(results)
                self.check(f"history-{commits}-transport-clean", not proxy.snapshot()["proxy_errors"])
            finally:
                proxy.close()
                for key, value in old_endpoint.items():
                    if value is None:
                        self.env.pop(key, None)
                    else:
                        self.env[key] = value
        self.check("qualifier-unchanged", sha256_file(self.args.qualifier) == self.report.artifacts["qualifier_sha256"])
        self.check_credential_disclosure()
        self.report.status = "passed"
        self.write_results(results)

    def write_results(self, results: list[dict]) -> None:
        output = self.artifacts / "read-scaling.json"
        output.write_text(json.dumps({"schema":1, "results":results}, indent=2) + "\n")
        self.report.artifacts["read_scaling"] = str(output)
        self.write_report()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--endpoint-url", required=True)
    parser.add_argument("--crab-bin", required=True)
    parser.add_argument("--qualifier", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--history", type=int, nargs="+", default=[100, 1000, 10000])
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--timeout", type=int, default=3600)
    args = parser.parse_args()
    if any(value <= 20 for value in args.history) or args.repeats < 1 or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.run_id):
        parser.error("require histories >20, positive repeats, and a safe run id")
    if not args.root.resolve().is_relative_to((Path.home() / "Workspace").resolve()):
        parser.error("qualification root must be on the mounted Workspace volume")
    args.root.mkdir(parents=True, exist_ok=True)
    (args.root / args.run_id).mkdir()
    args.access_key, args.secret_key, args.session_token = "crab", "crab", ""
    args.region, args.push_timeout, args.source = "us-east-1", args.timeout, None
    runner = ReadScalingRunner(args)
    try:
        runner.qualify()
    except Exception as error:
        runner.report.status = "failed"
        runner.report.artifacts["failure"] = str(error)
        raise
    finally:
        runner.write_report()


if __name__ == "__main__":
    main()
