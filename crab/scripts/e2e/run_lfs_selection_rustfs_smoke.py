#!/usr/bin/env python3
"""Compare Crab LFS selection with native Git LFS using isolated cold clones."""

import hashlib
import json
import os
from pathlib import Path

import run_protocol_v2_partial_clone_rustfs_smoke as runner


def cached_oids(repo):
    return sorted(path.name for path in (repo / ".git/lfs/objects").glob("*/*/*") if path.is_file())


def run(smoke):
    smoke.env.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
                      "GIT_LFS_SKIP_SMUDGE": "1"})
    health = smoke.endpoint_health()
    smoke.check("object-store-ready", health.get("ready") is True)
    smoke.ensure_bucket()
    native = smoke.run_cmd("native Git LFS version", ["git-lfs", "version"], smoke.run_root)
    smoke.report["git_lfs_version"] = smoke.stdout(native).strip()
    source = smoke.source
    smoke.run_git(smoke.run_root, ["init", "-b", "main", str(source)])
    smoke.run_cmd("initialize source", [str(smoke.crab_bin), "init", smoke.remote_url], source)
    smoke.run_cmd("install source filters", [str(smoke.crab_bin), "lfs", "install", "--local", "--skip-repo"], source)
    paths = ["models/a.bin", "models/private/b.bin", "other/c.bin"]
    (source / ".gitattributes").write_text("*.bin filter=lfs diff=lfs merge=lfs -text\n")
    history = set()
    current = {}
    for version in range(3):
        for name in paths:
            content = runner.deterministic_bytes(4096, f"{smoke.run_id}:{version}:{name}")
            target = source / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
            oid = hashlib.sha256(content).hexdigest()
            current[name] = oid
            history.add(oid)
        smoke.run_git(source, ["add", "."])
        smoke.run_git(source, ["commit", "-m", f"LFS selection version {version}"])
    revision = smoke.git_value(source, ["rev-parse", "HEAD"], name="fixture revision")
    smoke.record_provenance(revision, health)
    smoke.run_cmd("publish all LFS versions", [str(smoke.crab_bin), "lfs", "push", "--all", "origin"], source)
    smoke.run_git(source, ["push", "origin", "main"])

    def compare(label, operation, settings, flags, expected_paths=None, expected_oids=None):
        if expected_oids is None:
            expected_oids = sorted(current[name] for name in expected_paths)
        observations = {}
        for client in ("native", "crab"):
            name = f"{label}-{operation}-{client}"
            repo = smoke.run_root / name
            smoke.run_git(smoke.run_root, ["clone", smoke.remote_url, str(repo)], name=f"{name} clone")
            smoke.run_cmd(f"{name} install", [str(smoke.crab_bin), "lfs", "install", "--local", "--skip-smudge", "--skip-repo"], repo)
            smoke.check(f"{name} starts cold", cached_oids(repo) == [])
            for key, value in settings.items():
                smoke.run_git(repo, ["config", key, value], name=f"{name} {key}")
            command = ["git-lfs"] if client == "native" else [str(smoke.crab_bin), "lfs"]
            smoke.run_cmd(name, [*command, operation, *flags, "origin"], repo)
            observations[client] = cached_oids(repo)
            smoke.check(f"{name} exact object selection", observations[client] == expected_oids,
                        {"expected": expected_oids, "actual": observations[client]})
            if operation == "pull":
                hydrated = sorted(path for path in paths if hashlib.sha256((repo / path).read_bytes()).hexdigest() == current[path])
                smoke.check(f"{name} exact checkout selection", hydrated == sorted(expected_paths),
                            {"expected": sorted(expected_paths), "actual": hydrated})
        smoke.check(f"{label}-{operation} matches native Git LFS", observations["native"] == observations["crab"])

    settings = {"lfs.fetchinclude": "models", "lfs.fetchexclude": "models/private"}
    for label, flags, selected in (
        ("configured", [], [paths[0]]),
        ("override-include", ["--include", "other"], [paths[2]]),
        ("override-exclude", ["--exclude", "other"], paths[:2]),
        ("clear-include", ["--include", ""], [paths[0], paths[2]]),
        ("clear-exclude", ["--exclude", ""], paths[:2]),
        ("clear-both", ["--include", "", "--exclude", ""], paths),
    ):
        for operation in ("fetch", "pull"):
            compare(label, operation, settings, flags, expected_paths=selected)

    recent = {"lfs.fetchrecentalways": "true", "lfs.fetchrecentcommitsdays": "7"}
    compare("recent-always", "fetch", recent, [], expected_oids=sorted(history))
    compare("recent-always", "pull", recent, [], expected_paths=paths)
    recent["lfs.fetchrecentalways"] = "false"
    compare("recent-disabled", "fetch", recent, [], expected_paths=paths)
    compare("recent-explicit", "fetch", recent, ["--recent"], expected_oids=sorted(history))
    compare("all-ignores-defaults", "fetch", {**settings, **recent, "lfs.fetchrecentalways": "true"}, ["--all"], expected_oids=sorted(history))
    smoke.redaction_check()


def main():
    smoke = runner.ProtocolV2PartialCloneSmoke(runner.parse_args())
    digest = runner.sha256_file(smoke.crab_bin)
    smoke.report.update({"schema": "crab.lfs-selection-smoke", "version": "1.0",
                         "candidate_binary_sha256": digest,
                         "driver_sha256": runner.sha256_file(Path(__file__)),
                         "valid_for_comparison": False,
                         "comparison_invalid_reason": "Functional selection comparison, not performance."})
    try:
        run(smoke)
        smoke.check("candidate unchanged", runner.sha256_file(smoke.crab_bin) == digest)
        smoke.report["status"] = "passed"
    except Exception as error:
        smoke.report["status"] = "failed"
        smoke.report["error"] = runner.redact_text(str(error), smoke.credentials())
    finally:
        smoke.write_report()
    print(json.dumps({"status": smoke.report["status"], "error": smoke.report.get("error"),
                      "report": str(smoke.artifacts / "report.json")}))
    return 0 if smoke.report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
