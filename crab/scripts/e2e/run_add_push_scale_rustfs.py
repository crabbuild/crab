#!/usr/bin/env python3
"""Qualify add/push with 100-GiB content and cold cross-repo chunk reuse.

Uses a fresh bucket and disposable run directory. Ten distinct non-zero bases
are copied with filesystem copy-on-write, then edited independently. The default
workload is 100 GiB logical with 20 GiB of unique initial entropy, not sparse zeros.
Reuse the ordinary smoke runner's command logs, credentials, and S3 contracts.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any

from run_add_commit_push_rustfs_smoke import AddCommitPushSmoke, sha256_file
from run_concurrent_push_smoke import RequestCountingProxy

MIB = 1024 * 1024


def verify_parallel_proofs(runner: AddCommitPushSmoke, paths: list[Path]) -> None:
    source_cache = runner.env["CRAB_CACHE_DIR"]
    inventories = []
    for jobs in (1, 4):
        runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / f"proof-cache-{jobs}")
        repo, _, _ = runner.prepare_repo(f"proof-workers-{jobs}")
        for index, path in enumerate(paths[:4]):
            with path.open("rb") as source:
                (repo / f"probe-{index}.bin").write_bytes(source.read(16 * MIB))
        runner.run_crab(repo, ["add", "--jobs", str(jobs), "--jsonl", "*.bin"],
                        name=f"proof classification with {jobs} workers")
        inventories.append(runner.staging_payload_inventory(repo))
    runner.env["CRAB_CACHE_DIR"] = source_cache
    reused = (
        inventories[0]["recipe_remote_chunks"]
        + inventories[0]["prepared_payload_chunks"]
    )
    runner.check(
        "parallel-proof-classification-preserves-serial-coverage",
        reused > 0 and inventories[0] == inventories[1],
        {"serial": inventories[0], "parallel": inventories[1]},
    )


def write_transport_report(
    runner: AddCommitPushSmoke,
    records: list[dict[str, Any]],
    read_phases: list[dict[str, Any]],
    total: dict[str, Any],
) -> None:
    path = runner.artifacts / "capsule-xet-transport.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {"versions": records, "read_phases": read_phases, "total": total},
            indent=2,
            sort_keys=True,
        ) + "\n"
    )
    runner.report.artifacts["capsule_xet_transport"] = str(path)
    runner.write_report()


def measured_read(
    runner: AddCommitPushSmoke,
    proxy: RequestCountingProxy,
    read_phases: list[dict[str, Any]],
    repo: Path,
    args: list[str],
    name: str,
):
    before = proxy.snapshot()
    result = runner.run_crab(repo, args, name=name)
    read_phases.append({
        "name": name,
        "duration_ms": result.duration_ms,
        "transport": RequestCountingProxy.delta(before, proxy.snapshot()),
    })
    return result


def measured_hydrate(
    runner: AddCommitPushSmoke,
    proxy: RequestCountingProxy,
    read_phases: list[dict[str, Any]],
    repo: Path,
    name: str,
    payload_bytes: int,
    cache_bytes: int,
):
    cache_resident_bytes = 0
    cache_dir = getattr(runner, "env", {}).get("CRAB_CACHE_DIR")
    if cache_bytes and cache_dir and Path(cache_dir).is_dir():
        cache_stats = runner.run_crab(
            repo,
            ["cache", "stats", "--json"],
            name=f"{name} cache capacity inventory",
        )
        data = json.loads(runner.read_stdout(cache_stats))["data"]
        family = data.get("families", {}).get("decoded-range", {})
        if (
            data.get("scan_complete") is True
            and family.get("complete") is True
            and family.get("issues") == 0
        ):
            cache_resident_bytes = min(
                cache_bytes,
                max(0, int(family.get("allocated_bytes", 0))),
            )
    cache_growth_bytes = cache_bytes - cache_resident_bytes
    required = payload_bytes + cache_growth_bytes + 20 * 1024**3
    available = shutil.disk_usage(runner.args.root).free
    runner.check(
        f"{name} capacity", available >= required,
        {
            "required_bytes": required,
            "cache_resident_bytes": cache_resident_bytes,
            "cache_growth_bytes": cache_growth_bytes,
            "available_bytes": available,
        },
    )
    return measured_read(runner, proxy, read_phases, repo, ["hydrate", "--all"], name)


def verify_no_proxy_errors(runner: AddCommitPushSmoke, proxy: RequestCountingProxy) -> None:
    errors = proxy.snapshot(include_paths=False)["proxy_errors"]
    runner.check("request-meter-no-proxy-errors", not errors, {"proxy_errors": errors})


def object_inventory(runner: AddCommitPushSmoke, prefix: str) -> dict[str, int]:
    payload = runner.aws_json(
        f"inventory {prefix}",
        ["list-objects-v2", "--bucket", runner.args.bucket, "--prefix", prefix],
    )
    if payload.get("IsTruncated"):
        raise RuntimeError(f"inventory exceeded one page: {prefix}")
    entries = payload.get("Contents", [])
    return {
        "objects": len(entries),
        "bytes": sum(int(entry.get("Size", 0)) for entry in entries),
    }


def validate_capacity_stop_report(args: argparse.Namespace) -> tuple[dict[str, Any], str]:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.run_id):
        raise ValueError("capacity stop run id is unsafe")
    run_root = args.root / args.run_id
    report_path = run_root / "artifacts" / "report.json"
    if run_root.is_symlink() or not run_root.is_dir() or report_path.is_symlink():
        raise ValueError("capacity stop report is missing or unsafe")
    try:
        report_bytes = report_path.read_bytes()
        report = json.loads(report_bytes)
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError("capacity stop report is unreadable") from error
    artifacts_dir = run_root / "artifacts"
    if (not isinstance(report, dict) or artifacts_dir.is_symlink()
            or run_root.resolve().parent != args.root.resolve()):
        raise ValueError("capacity stop report is missing or unsafe")

    if report.get("run_id") != args.run_id:
        raise ValueError("capacity stop run id does not match")
    if Path(report.get("root", "")).resolve() != run_root.resolve():
        raise ValueError("capacity stop root does not match")
    if report.get("bucket") != args.bucket:
        raise ValueError("capacity stop bucket does not match")
    if report.get("endpoint_url") != args.endpoint_url:
        raise ValueError("capacity stop endpoint does not match")
    if report.get("status") != "failed":
        raise ValueError("capacity stop report is not a failed run")

    artifacts = report.get("artifacts")
    checks = report.get("checks")
    if (not isinstance(artifacts, dict) or not isinstance(checks, list) or not checks
            or any(not isinstance(check, dict) for check in checks)
            or any(check.get("ok") is not True for check in checks[:-1])
            or checks[-1].get("name") != "rehydrated hydrate capacity"
            or checks[-1].get("ok") is not False
            or artifacts.get("failure") != "check failed: rehydrated hydrate capacity"):
        raise ValueError("report is not a terminal rehydrated hydrate capacity stop")

    workload_checks = [item for item in checks if item.get("name") == "workload-shape"]
    if len(workload_checks) != 1 or workload_checks[0].get("ok") is not True:
        raise ValueError("capacity stop workload evidence is missing")
    workload = workload_checks[0].get("detail")
    if not isinstance(workload, dict):
        raise ValueError("capacity stop workload evidence is invalid")
    if (
        workload.get("large_files") != args.files
        or workload.get("logical_bytes") != args.files * args.file_mib * MIB
        or workload.get("small_code_files") != args.code_files
        or workload.get("versions") != args.versions
    ):
        raise ValueError("capacity stop workload does not match")

    binary_value = artifacts.get("crab_binary")
    if not isinstance(binary_value, str):
        raise ValueError("capacity stop Crab binary identity is missing")
    binary_path = Path(binary_value)
    binary_sha256 = artifacts.get("crab_binary_sha256")
    source_head_sha = artifacts.get("source_head_sha")
    if (not binary_path.is_file() or binary_path.is_symlink()
            or binary_sha256 != sha256_file(binary_path)
            or Path(args.crab_bin).resolve() != binary_path.resolve()):
        raise ValueError("capacity stop Crab binary identity does not match")
    if not isinstance(source_head_sha, str) or not re.fullmatch(r"[0-9a-f]{40}", source_head_sha):
        raise ValueError("capacity stop source revision is missing")

    def artifact_path(name: str) -> Path:
        raw = artifacts.get(name)
        path = Path(raw) if isinstance(raw, str) else Path()
        if (not path.is_absolute() or path.is_symlink() or not path.is_file()
                or path.resolve().parent != artifacts_dir.resolve()):
            raise ValueError(f"capacity stop {name} artifact is missing or unsafe")
        return path

    history_path = artifact_path("expected_history")
    transport_path = artifact_path("capsule_xet_transport")
    expected_path = artifacts_dir / "expected-sha256.json"
    if expected_path.is_symlink() or not expected_path.is_file():
        raise ValueError("capacity stop expected SHA-256 inventory is missing")
    try:
        history = json.loads(history_path.read_text(encoding="utf-8"))
        expected = json.loads(expected_path.read_text(encoding="utf-8"))
        transport = json.loads(transport_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError("capacity stop verification artifacts are unreadable") from error

    expected_file_count = args.files + args.code_files
    if not isinstance(history, list) or len(history) != args.versions or not isinstance(expected, dict):
        raise ValueError("capacity stop history inventory is incomplete")
    for version, snapshot in enumerate(history):
        if not isinstance(snapshot, dict):
            raise ValueError("capacity stop history inventory is invalid")
        files = snapshot.get("files")
        if (snapshot.get("version") != version
                or not isinstance(snapshot.get("generation"), int)
                or snapshot["generation"] < 0
                or not isinstance(snapshot.get("commit"), str)
                or not re.fullmatch(r"[0-9a-f]{40}", snapshot["commit"])
                or not isinstance(snapshot.get("digest"), str)
                or not re.fullmatch(r"[0-9a-f]{64}", snapshot["digest"])
                or not isinstance(files, dict) or len(files) != expected_file_count
                or any(not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest)
                       for digest in files.values())):
            raise ValueError("capacity stop history inventory is invalid")
        if (sum(path.startswith("models/") for path in files) != args.files
                or sum(path.startswith("src/") for path in files) != args.code_files):
            raise ValueError("capacity stop history file inventory does not match the workload")
    if expected != history[-1]["files"]:
        raise ValueError("capacity stop final byte inventory does not match history")
    if (not isinstance(transport, dict)
            or not isinstance(transport.get("total"), dict)
            or transport["total"].get("proxy_errors") != {}):
        raise ValueError("capacity stop request meter recorded proxy errors")

    commands = report.get("commands", [])
    clone = run_root / "clone"
    repo = run_root / "scale" / "repo"
    if (not isinstance(commands, list) or not commands
            or any(not isinstance(command, dict) for command in commands)
            or commands[-1].get("name") != "cold dehydrate"
            or commands[-1].get("exit_code") != 0
            or Path(commands[-1].get("cwd", "")).resolve() != clone.resolve()
            or any(command.get("name") == "rehydrated hydrate" for command in commands)
            or clone.is_symlink() or repo.is_symlink()
            or (clone / ".git").is_symlink() or (repo / ".git").is_symlink()
            or not (clone / ".git").exists() or not (repo / ".git").exists()):
        raise ValueError("capacity stop is not safe to resume from the cold-dehydrated clone")

    return report, hashlib.sha256(report_bytes).hexdigest()


def write_resume_transport_report(
    runner: AddCommitPushSmoke,
    prior_report_sha256: str,
    prior_transport_sha256: str,
    records: list[dict[str, Any]],
    read_phases: list[dict[str, Any]],
    total: dict[str, Any],
) -> None:
    path = runner.artifacts / "capsule-xet-transport.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps({
            "scope": "capacity-stop continuation only",
            "prior_report_sha256": prior_report_sha256,
            "prior_transport_sha256": prior_transport_sha256,
            "versions": records,
            "read_phases": read_phases,
            "total": total,
        }, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    runner.report.artifacts["capsule_xet_transport"] = str(path)
    runner.write_report()


def release_verified_run_child(runner: AddCommitPushSmoke, path: Path) -> None:
    if (path.parent != runner.run_root or path.is_symlink()
            or path.resolve().parent != runner.run_root.resolve()):
        raise ValueError("refusing to release output outside this qualification run")
    if path.exists():
        shutil.rmtree(path)


def verify_hydrated_clone(
    runner: AddCommitPushSmoke,
    proxy: RequestCountingProxy,
    read_phases: list[dict[str, Any]],
    clone: Path,
    expected: dict[str, str],
    large_files: list[str],
    logical_bytes: int,
    cache_bytes: int,
    cycle: str,
) -> None:
    measured_hydrate(
        runner, proxy, read_phases, clone, f"{cycle} hydrate", logical_bytes, cache_bytes
    )
    for relative, digest in expected.items():
        runner.check(f"{cycle}-bytes-{relative}", sha256_file(clone / relative) == digest)
    runner.run_crab(clone, ["dehydrate", "--all"], name=f"{cycle} dehydrate")
    for relative in large_files:
        pointer = clone / relative
        runner.check(
            f"{cycle}-pointer-{pointer.name}",
            pointer.stat().st_size < 1024
            and pointer.read_text().startswith("version https://crab.build/spec/v1"),
        )


def verify_history_and_restore(
    args: argparse.Namespace,
    runner: AddCommitPushSmoke,
    proxy: RequestCountingProxy,
    transport_records: list[dict[str, Any]],
    read_phases: list[dict[str, Any]],
    repo: Path,
    remote: str,
    clone: Path,
    history: list[dict[str, Any]],
    logical_bytes: int,
    cache_bytes: int,
    *,
    output_prefix: str = "",
    preserve_cold_clone_cache: bool = False,
    resume_metadata: tuple[str, str] | None = None,
) -> None:
    def output_name(name: str) -> str:
        return f"{output_prefix}-{name}" if output_prefix else name

    runner.run_git(clone, ["fsck", "--full", "--strict"])
    if not preserve_cold_clone_cache:
        release_verified_run_child(runner, runner.run_root / "cold-clone-cache")
    for snapshot in history:
        version = snapshot["version"]
        cache_name = output_name(f"history-cache-{version}")
        runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / cache_name)
        runner.run_git(clone, ["checkout", "--detach", snapshot["commit"]],
                       name=f"v{version} historical checkout")
        measured_hydrate(runner, proxy, read_phases, clone, f"v{version} historical hydrate",
                         logical_bytes, cache_bytes)
        for relative, digest in snapshot["files"].items():
            runner.check(f"v{version}-historical-bytes-{relative}", sha256_file(clone / relative) == digest)
        runner.run_crab(clone, ["dehydrate", "--all"], name=f"v{version} historical dehydrate")
        verified = measured_read(
            runner, proxy, read_phases, repo,
            ["recover", "history", "verify", str(snapshot["generation"]),
             "--digest", snapshot["digest"], "--json"],
            f"v{version} retained history integrity",
        )
        proof = json.loads(runner.read_stdout(verified))["data"]
        runner.check(f"v{version}-history-verification-exact",
                     proof["generation"] == snapshot["generation"]
                     and proof["digest"] == snapshot["digest"]
                     and proof["xorbs"] > 0 and proof["shards"] > 0, proof)
        release_verified_run_child(runner, runner.run_root / cache_name)

    oldest = history[0]
    external_before = {
        prefix: runner.list_keys(prefix) for prefix in (".crab/xorbs/", ".crab/shards/")
    }
    restored = measured_read(
        runner, proxy, read_phases, repo,
        ["recover", "history", "restore", str(oldest["generation"]),
         "--digest", oldest["digest"], "--apply", "--json"],
        "restore oldest retained Xet history",
    )
    runner.check("history-restore-applied", json.loads(runner.read_stdout(restored))["data"]["applied"])
    runner.check("history-restore-exact-tip",
                 runner.ls_remote(remote, name="restored refs").get("refs/heads/main") == oldest["commit"])
    for prefix, keys in external_before.items():
        runner.check(f"history-restore-preserves-{prefix}", runner.list_keys(prefix) == keys)

    restored_clone_name = output_name("restored-clone")
    restored_clone = runner.run_root / restored_clone_name
    restored_cache_name = output_name("restored-clone-cache")
    republished_cache_name = output_name("republished-clone-cache")
    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / restored_cache_name)
    runner.run_cmd("restored history clone", [runner.crab_bin, "clone", remote, str(restored_clone)], runner.run_root)
    for stage, snapshot in (("restored", oldest), ("republished", history[-1])):
        if stage == "republished":
            runner.run_crab(repo, ["push", "origin", "HEAD:refs/heads/main"],
                            name="publish current version after history restore")
            runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / republished_cache_name)
            runner.run_git(restored_clone, ["fetch", "origin"], name="fetch after restore and publication")
            runner.run_git(restored_clone, ["checkout", "--detach", "refs/remotes/origin/main"])
        runner.check(f"{stage}-clone-exact-tip", runner.rev_parse(restored_clone, "HEAD") == snapshot["commit"])
        measured_hydrate(runner, proxy, read_phases, restored_clone, f"{stage} history hydrate",
                         logical_bytes, cache_bytes)
        for relative, digest in snapshot["files"].items():
            runner.check(f"{stage}-history-bytes-{relative}", sha256_file(restored_clone / relative) == digest)
        runner.run_git(restored_clone, ["fsck", "--full", "--strict"], name=f"{stage} history Git integrity")
        runner.run_crab(restored_clone, ["dehydrate", "--all"], name=f"{stage} history dehydrate")
        cache_name = restored_cache_name if stage == "restored" else republished_cache_name
        release_verified_run_child(runner, runner.run_root / cache_name)

    fsck = measured_read(runner, proxy, read_phases, repo, ["fsck", "--json"],
                         "layered Xet remote fsck")
    fsck_data = json.loads(runner.read_stdout(fsck))["data"]
    runner.check(
        "layered-xet-remote-fsck-clean",
        fsck_data["passed"] and fsck_data["errors"] == 0 and fsck_data["repair_failures"] == 0,
        fsck_data,
    )
    verify_no_proxy_errors(runner, proxy)
    runner.check("binary-unchanged", sha256_file(Path(runner.crab_bin)) == runner.report.artifacts["crab_binary_sha256"])
    runner.check_credential_disclosure()
    runner.report.status = "passed"
    if resume_metadata is None:
        write_transport_report(runner, transport_records, read_phases, proxy.snapshot())
    else:
        write_resume_transport_report(
            runner, resume_metadata[0], resume_metadata[1], transport_records,
            read_phases, proxy.snapshot(),
        )
    runner.write_report()


def run(args: argparse.Namespace) -> None:
    if getattr(args, "resume_capacity_stop", False):
        resume_capacity_stop(args)
        return
    proxy = RequestCountingProxy(args.endpoint_url, args.bucket)
    proxy.start()
    runner = AddCommitPushSmoke(args)
    runner.report.artifacts["scale_harness_sha256"] = sha256_file(Path(__file__))
    runner.report.artifacts["request_meter_sha256"] = sha256_file(
        Path(__file__).with_name("run_concurrent_push_smoke.py")
    )
    runner.env["AWS_ENDPOINT_URL"] = proxy.url
    runner.env["AWS_ENDPOINT_URL_S3"] = proxy.url
    records: list[dict[str, Any]] = []
    read_phases: list[dict[str, Any]] = []
    if runner.run_root.exists():
        proxy.close()
        raise RuntimeError("use a fresh run directory")
    try:
        verify(args, runner, proxy, records, read_phases)
    except Exception as error:
        runner.report.status = "failed"
        runner.report.artifacts["failure"] = str(error)
        write_transport_report(runner, records, read_phases, proxy.snapshot())
        runner.write_report()
        raise
    finally:
        proxy.close()


def resume_capacity_stop(args: argparse.Namespace) -> None:
    if getattr(args, "cleanup", False):
        raise ValueError("capacity-stop resume cannot clean up the qualification run")
    report, report_sha256 = validate_capacity_stop_report(args)
    run_root = args.root / args.run_id
    report_path = run_root / "artifacts" / "report.json"
    prior_transport_path = Path(report["artifacts"]["capsule_xet_transport"])
    prior_transport_bytes = prior_transport_path.read_bytes()
    prior_transport_sha256 = hashlib.sha256(prior_transport_bytes).hexdigest()
    prior_transport = json.loads(prior_transport_bytes)
    history = json.loads(Path(report["artifacts"]["expected_history"]).read_text(encoding="utf-8"))
    expected = json.loads((run_root / "artifacts" / "expected-sha256.json").read_text(encoding="utf-8"))
    if not getattr(args, "release_cold_clone_cache_after_rehydration", False):
        logical_bytes = args.files * args.file_mib * MIB
        cache_bytes = min(10, args.files) * args.file_mib * MIB
        required_for_history = logical_bytes + cache_bytes + 20 * 1024**3
        available = shutil.disk_usage(args.root).free
        if available < required_for_history:
            raise ValueError(
                "capacity-stop resume requires at least "
                f"{required_for_history} free bytes for isolated history hydration; "
                "provide more space or explicitly authorize releasing the cold-clone cache"
            )

    attempt = 1
    while True:
        attempt_name = f"resume-{attempt}"
        attempt_root = run_root / attempt_name
        try:
            attempt_root.mkdir(mode=0o700)
            break
        except FileExistsError:
            attempt += 1

    runner = AddCommitPushSmoke(args)
    runner.logs = attempt_root / "logs"
    runner.artifacts = attempt_root / "artifacts"
    runner.report.run_id = f"{args.run_id}-{attempt_name}"
    runner.report.root = str(run_root)
    runner.report.artifacts.update({
        "resume_scope": "rehydrated hydrate and retained-history continuation",
        "resume_parent_report": str(report_path),
        "resume_parent_report_sha256": report_sha256,
        "resume_parent_transport": str(prior_transport_path),
        "resume_parent_transport_sha256": prior_transport_sha256,
        "source_head_sha": report["artifacts"]["source_head_sha"],
        "crab_binary": runner.crab_bin,
        "crab_binary_sha256": report["artifacts"]["crab_binary_sha256"],
        "scale_harness_sha256": sha256_file(Path(__file__)),
        "request_meter_sha256": sha256_file(Path(__file__).with_name("run_concurrent_push_smoke.py")),
    })

    proxy = RequestCountingProxy(args.endpoint_url, args.bucket)
    records: list[dict[str, Any]] = []
    read_phases: list[dict[str, Any]] = []
    runner.write_report()
    try:
        proxy.start()
        runner.env["AWS_ENDPOINT_URL"] = proxy.url
        runner.env["AWS_ENDPOINT_URL_S3"] = proxy.url
        remote, _ = runner.remote_for_case("scale")
        repo = run_root / "scale" / "repo"
        clone = run_root / "clone"
        latest_commit = history[-1]["commit"]
        runner.check(
            "resume-parent-request-meter-clean",
            not prior_transport["total"]["proxy_errors"],
            {"proxy_errors": prior_transport["total"]["proxy_errors"]},
        )
        runner.check("resume-source-tip-unchanged", runner.rev_parse(repo, "HEAD") == latest_commit)
        runner.check("resume-clone-tip-unchanged", runner.rev_parse(clone, "HEAD") == latest_commit)
        runner.check(
            "resume-remote-tip-unchanged",
            runner.ls_remote(remote, name="resume remote refs").get("refs/heads/main") == latest_commit,
        )
        runner.env["CRAB_CACHE_DIR"] = str(run_root / "cold-clone-cache")
        large_files = [path for path in expected if path.startswith("models/")]
        logical_bytes = args.files * args.file_mib * MIB
        cache_bytes = min(10, args.files) * args.file_mib * MIB
        verify_hydrated_clone(
            runner, proxy, read_phases, clone, expected, large_files,
            logical_bytes, cache_bytes, "rehydrated",
        )
        verify_history_and_restore(
            args, runner, proxy, records, read_phases, repo, remote, clone,
            history, logical_bytes, cache_bytes,
            output_prefix=attempt_name,
            preserve_cold_clone_cache=not getattr(
                args, "release_cold_clone_cache_after_rehydration", False
            ),
            resume_metadata=(report_sha256, prior_transport_sha256),
        )
        runner.check(
            "resume-parent-report-unchanged",
            sha256_file(report_path) == report_sha256,
            {"sha256": report_sha256},
        )
        runner.check(
            "resume-parent-transport-unchanged",
            sha256_file(prior_transport_path) == prior_transport_sha256,
            {"sha256": prior_transport_sha256},
        )
    except Exception as error:
        runner.report.status = "failed"
        runner.report.artifacts["failure"] = str(error)
        write_resume_transport_report(
            runner, report_sha256, prior_transport_sha256,
            records, read_phases, proxy.snapshot(),
        )
        runner.write_report()
        raise
    finally:
        proxy.close()


def verify(
    args: argparse.Namespace,
    runner: AddCommitPushSmoke,
    proxy: RequestCountingProxy,
    transport_records: list[dict[str, Any]],
    read_phases: list[dict[str, Any]],
) -> None:
    status, _, _ = runner.signed_s3_request("HEAD", "")
    runner.check("fresh-bucket", status == 404, {"head_status": status})
    runner.preflight()
    scratch = runner.run_root / "tmp"
    scratch.mkdir()
    runner.env["TMPDIR"] = str(scratch)
    logical_bytes = args.files * args.file_mib * MIB
    distinct_basis_bytes = min(10, args.files) * args.file_mib * MIB
    # Completed-phase caches are released before the next large hydration.
    # Conservatively allow one hydrated checkout plus source, staging, origin,
    # active cache and transient work; the former two-copy estimate ran out.
    required = logical_bytes + 5 * distinct_basis_bytes + 20 * 1024**3
    available = shutil.disk_usage(args.root).free
    runner.check(
        "disk-capacity", available >= required,
        {"required_bytes": required, "logical_bytes": logical_bytes,
         "distinct_basis_bytes": distinct_basis_bytes, "available_bytes": available},
    )
    repo, remote, repo_prefix = runner.prepare_repo("scale")
    outside = runner.run_root / "symlink-target"
    outside.mkdir()
    (outside / "model.bin").write_bytes(b"external bytes must not enter staging")
    (repo / "linked").symlink_to(outside, target_is_directory=True)
    runner.run_crab(repo, ["add", "linked/model.bin"], name="symlink ancestor add", check=False)
    index = repo / ".crab" / "staging" / "index.db"
    inventory = runner.staging_payload_inventory(repo) if index.exists() else {}
    runner.check("symlink-ancestor-created-no-payload", not any(inventory.values()), inventory)
    size = args.file_mib * MIB
    models = repo / "models"
    models.mkdir()
    paths = [models / f"model-{index:03}.bin" for index in range(args.files)]
    families = min(10, len(paths))
    available_before = shutil.disk_usage(args.root).free
    for index, path in enumerate(paths):
        if index < families:
            with path.open("wb") as stream:
                for block in range(args.file_mib):
                    stream.write(hashlib.shake_256(f"family:{index}:{block}".encode()).digest(MIB))
        else:
            clone_flag = "-c" if sys.platform == "darwin" else "--reflink=always"
            runner.run_cmd(f"copy-on-write fixture {index}",
                           ["cp", clone_flag, str(paths[index % families]), str(path)], repo)
    code = repo / "src"
    code.mkdir()
    for index in range(args.code_files):
        (code / f"module_{index:04}.rs").write_text(f"pub const VALUE: u64 = {index};\n")
    runner.check("workload-shape", True, {
        "large_files": len(paths), "logical_bytes": sum(p.stat().st_size for p in paths),
        "distinct_basis_bytes": families * size,
        "allocated_file_bytes_including_shared_extents": sum(p.stat().st_blocks * 512 for p in paths),
        "observed_free_space_delta": available_before - shutil.disk_usage(args.root).free,
        "small_code_files": args.code_files, "versions": args.versions,
    })
    history: list[dict[str, Any]] = []
    for version in range(args.versions):
        if version:
            for index, path in enumerate(paths):
                with path.open("r+b") as stream:
                    stream.seek(size // 2 + version * MIB)
                    stream.write(hashlib.shake_256(f"edit:{version}:{index}".encode()).digest(MIB))
            for index in range(args.code_files):
                with (code / f"module_{index:04}.rs").open("a") as stream:
                    stream.write(f"pub const VERSION_{version}: u64 = {version};\n")
        if version == 1:
            selected = str(paths[0].relative_to(repo))
            runner.run_crab(repo, ["add", "--jsonl", "--skip-git-add", selected], name="deferred preparation")
            runner.run_git(repo, ["add", selected], name="deferred Git publication")
        if version == 2 and len(paths) > 1:
            with ThreadPoolExecutor(max_workers=2) as pool:
                pending = [pool.submit(runner.run_crab, repo,
                           ["add", "--jsonl", str(path.relative_to(repo))], name=f"concurrent add {path.name}")
                           for path in paths[:2]]
                for result in pending:
                    result.result()
        before_add = proxy.snapshot()
        add = runner.run_crab(
            repo, ["add", "--jsonl", "models/**"], name=f"v{version} add"
        )
        add_transport = RequestCountingProxy.delta(before_add, proxy.snapshot())
        for path in paths:
            runner.assert_index_pointer(repo, str(path.relative_to(repo)), size)
        runner.run_git(repo, ["add", "src"])
        runner.run_git(repo, ["commit", "-m", f"version {version}"])
        before_push = proxy.snapshot()
        push = runner.run_crab(
            repo,
            ["push", "--jsonl", "origin", "HEAD:refs/heads/main"],
            name=f"v{version} push",
            timeout=args.push_timeout,
        )
        push_transport = RequestCountingProxy.delta(before_push, proxy.snapshot())
        transport_records.append(
            {
                "version": version,
                "add_duration_ms": add.duration_ms,
                "add": add_transport,
                "push_duration_ms": push.duration_ms,
                "push": push_transport,
                "xorbs": object_inventory(runner, ".crab/xorbs/"),
                "shards": object_inventory(runner, ".crab/shards/"),
            }
        )
        write_transport_report(runner, transport_records, read_phases, proxy.snapshot())
        if version == 0:
            runner.check(
                "capsule-root-published",
                bool(runner.list_keys(f"{repo_prefix}/v2/root")),
                {"repo_prefix": repo_prefix},
            )
            runner.check(
                "capsule-run-published",
                bool(runner.list_keys(f"{repo_prefix}/v2/capsules/")),
                {"repo_prefix": repo_prefix},
            )
            verify_parallel_proofs(runner, paths)

        expected = {
            str(path.relative_to(repo)): sha256_file(path)
            for path in [*paths, *sorted(code.iterdir())]
        }
        history.append({"version": version, "commit": runner.rev_parse(repo, "HEAD"), "files": expected})
        history_path = runner.artifacts / "expected-history-sha256.json"
        history_path.write_text(json.dumps(history, indent=2) + "\n")
        runner.report.artifacts["expected_history"] = str(history_path)

        refs_before = runner.ls_remote(remote, name=f"v{version} refs before repack")
        external_before = {
            prefix: runner.list_keys(prefix) for prefix in (".crab/xorbs/", ".crab/shards/")
        }
        before_repack = proxy.snapshot()
        repack = runner.run_crab(repo, ["repack", "--jsonl"], name=f"v{version} layered repack")
        transport_records[-1]["repack"] = RequestCountingProxy.delta(before_repack, proxy.snapshot())
        transport_records[-1]["repack_duration_ms"] = repack.duration_ms
        runner.check(
            f"v{version}-repack-preserves-refs",
            runner.ls_remote(remote, name=f"v{version} refs after repack") == refs_before,
        )
        for prefix, keys in external_before.items():
            runner.check(f"v{version}-repack-preserves-{prefix}", runner.list_keys(prefix) == keys)
        retained = runner.run_crab(repo, ["recover", "history", "list", "--json"],
                                   name=f"v{version} retained history")
        entries = json.loads(runner.read_stdout(retained))["data"]["entries"]
        runner.check(f"v{version}-retained-history-present", bool(entries))
        latest = max(entries, key=lambda entry: entry["generation"])
        history[-1].update({"generation": latest["generation"], "digest": latest["digest"]})
        history_path.write_text(json.dumps(history, indent=2) + "\n")
        write_transport_report(runner, transport_records, read_phases, proxy.snapshot())

    initial = transport_records[0]
    final = transport_records[-1]
    logical_history_bytes = args.files * size * args.versions
    retained_ratio = final["xorbs"]["bytes"] / logical_history_bytes
    runner.check(
        "versioned-xet-content-is-deduplicated",
        initial["xorbs"]["objects"] > 0
        and final["shards"]["objects"] >= args.versions
        and retained_ratio < 0.25,
        {
            "logical_history_bytes": logical_history_bytes,
            "unique_xorb_bytes": final["xorbs"]["bytes"],
            "retained_ratio": retained_ratio,
            "versions": args.versions,
        },
    )
    release_verified_run_child(runner, runner.cache_dir)

    expected = history[-1]["files"]
    (runner.artifacts / "expected-sha256.json").write_text(json.dumps(expected, indent=2))

    # No source cache or prepared proof can mask the consumer's remote lookup.
    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "consumer-cache")
    consumer, consumer_remote, _ = runner.prepare_repo("cold-consumer")
    consumer_file = consumer / "model.bin"
    with paths[0].open("rb") as source, consumer_file.open("wb") as output:
        for _ in range(512):
            output.write(source.read(MIB))
        output.write(hashlib.shake_256(b"consumer-only-tail").digest(MIB))
    consumer_digest = sha256_file(consumer_file)
    consumer_bytes = consumer_file.stat().st_size
    # The consumer fixture has copied the source bytes; keep the source Git
    # repository for recovery checks without retaining another hydrated copy.
    runner.run_crab(repo, ["dehydrate", "--all"], name="dehydrate published source")
    for path in paths:
        is_pointer = path.stat().st_size < 1024
        runner.check(f"source-dehydrated-{path.name}",
                     is_pointer and path.read_text().startswith("version https://crab.build/spec/v1"))
    runner.run_git(repo, ["diff", "--quiet", "HEAD", "--", "models"],
                   name="dehydrated source preserves Git index")
    runner.run_git(consumer, ["add", "model.bin"])
    inventory = runner.staging_payload_inventory(consumer)
    runner.check("cold-consumer-exceeds-one-candidate-page", inventory["chunk_payloads"] > 4096, inventory)
    runner.run_git(consumer, ["commit", "-m", "reuse chunks with a distinct file hash"])
    before = runner.list_keys(".crab/xorbs/")
    before_inventory = object_inventory(runner, ".crab/xorbs/")
    runner.run_crab(consumer, ["push", "--log-level", "debug", "origin", "HEAD:refs/heads/main"],
                    name="cold cross-repository push", timeout=args.push_timeout)
    added = runner.list_keys(".crab/xorbs/") - before
    after_inventory = object_inventory(runner, ".crab/xorbs/")
    added_bytes = after_inventory["bytes"] - before_inventory["bytes"]
    # Appending changes the prior EOF chunk boundary, so the terminal xorb and
    # the tail may both be new even when every stable source chunk is reused.
    runner.check("cold-consumer-reuses-shared-chunks",
                 len(added) <= 2 and added_bytes < consumer_file.stat().st_size // 4,
                 {"new_xorbs": len(added), "new_xorb_bytes": added_bytes,
                  "logical_bytes": consumer_file.stat().st_size})
    # The consumer's published bytes are now proven by origin inventory; its
    # source worktree is no longer needed before the independent clone check.
    release_verified_run_child(runner, consumer.parent)
    release_verified_run_child(runner, runner.run_root / "consumer-cache")
    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "consumer-clone-cache")
    consumer_clone = runner.run_root / "consumer-clone"
    runner.run_cmd("consumer clone", [runner.crab_bin, "clone", consumer_remote, str(consumer_clone)], runner.run_root)
    measured_hydrate(runner, proxy, read_phases, consumer_clone, "consumer hydrate",
                     consumer_bytes, consumer_bytes)
    runner.check("consumer-byte-identity", sha256_file(consumer_clone / "model.bin") == consumer_digest)
    release_verified_run_child(runner, consumer_clone)
    release_verified_run_child(runner, runner.run_root / "consumer-clone-cache")

    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "cold-clone-cache")
    clone = runner.run_root / "clone"
    runner.run_cmd("scale clone", [runner.crab_bin, "clone", remote, str(clone)], runner.run_root)
    large_files = [str(path.relative_to(repo)) for path in paths]
    for cycle in ("cold", "rehydrated"):
        verify_hydrated_clone(
            runner, proxy, read_phases, clone, expected, large_files,
            logical_bytes, distinct_basis_bytes, cycle,
        )
    verify_history_and_restore(
        args, runner, proxy, transport_records, read_phases, repo, remote, clone,
        history, logical_bytes, distinct_basis_bytes,
    )
    if args.cleanup:
        # All targets were created by this invocation; retain reports and logs.
        restored_clone = runner.run_root / "restored-clone"
        for path in (repo.parent, consumer.parent, clone, consumer_clone, restored_clone,
                     runner.run_root / "restored-clone-cache", runner.run_root / "republished-clone-cache",
                     runner.cache_dir, runner.run_root / "consumer-cache",
                     runner.run_root / "consumer-clone-cache", runner.run_root / "cold-clone-cache", outside,
                     runner.run_root / "proof-cache-1", runner.run_root / "proof-cache-4",
                     runner.run_root / "proof-workers-1", runner.run_root / "proof-workers-4",
                     *[runner.run_root / f"history-cache-{item['version']}" for item in history], scratch):
            if path.exists():
                shutil.rmtree(path)
        runner.run_cmd("clean isolated bucket", ["aws", "--endpoint-url", args.endpoint_url,
                       "s3", "rb", f"s3://{args.bucket}", "--force"], runner.run_root)
        runner.check("isolated-data-cleaned", True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--crab-bin", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--endpoint-url", default="http://127.0.0.1:9000")
    parser.add_argument("--files", type=int, default=50)
    parser.add_argument("--file-mib", type=int, default=2048)
    parser.add_argument("--code-files", type=int, default=500)
    parser.add_argument("--versions", type=int, default=3)
    parser.add_argument("--cleanup", action="store_true")
    parser.add_argument(
        "--resume-capacity-stop",
        action="store_true",
        help="continue only a verified terminal rehydrated-hydrate capacity stop",
    )
    parser.add_argument(
        "--release-cold-clone-cache-after-rehydration",
        action="store_true",
        help="delete only the original cold-clone cache after hydrated bytes and Git fsck pass",
    )
    args = parser.parse_args()
    if args.files < 1 or args.file_mib < 500 or not 1 <= args.versions <= 11 or args.code_files < 1:
        parser.error("require positive file counts, >=500 MiB/file, and 1–11 versions")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.run_id):
        parser.error("run-id must be a single safe directory name")
    if args.resume_capacity_stop and args.cleanup:
        parser.error("--cleanup is not permitted when resuming a capacity stop")
    if args.release_cold_clone_cache_after_rehydration and not args.resume_capacity_stop:
        parser.error("cache release is only valid with --resume-capacity-stop")
    args.access_key = "crab"
    args.secret_key = "crab"
    args.session_token = ""
    args.region = "us-east-1"
    args.timeout = 3600
    args.push_timeout = 3600
    args.source = None
    run(args)


if __name__ == "__main__":
    main()
