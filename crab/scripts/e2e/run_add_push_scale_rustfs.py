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
    required = payload_bytes + cache_bytes + 20 * 1024**3
    available = shutil.disk_usage(runner.args.root).free
    runner.check(
        f"{name} capacity", available >= required,
        {"required_bytes": required, "available_bytes": available},
    )
    return measured_read(runner, proxy, read_phases, repo, ["hydrate", "--all"], name)


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


def release_verified_run_child(runner: AddCommitPushSmoke, path: Path) -> None:
    if (path.parent != runner.run_root or path.is_symlink()
            or path.resolve().parent != runner.run_root.resolve()):
        raise ValueError("refusing to release output outside this qualification run")
    if path.exists():
        shutil.rmtree(path)


def run(args: argparse.Namespace) -> None:
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
    for cycle in ("cold", "rehydrated"):
        measured_hydrate(runner, proxy, read_phases, clone, f"{cycle} hydrate",
                         logical_bytes, distinct_basis_bytes)
        for relative, digest in expected.items():
            runner.check(f"{cycle}-bytes-{relative}", sha256_file(clone / relative) == digest)
        runner.run_crab(clone, ["dehydrate", "--all"], name=f"{cycle} dehydrate")
        for path in paths:
            pointer = clone / path.relative_to(repo)
            runner.check(f"{cycle}-pointer-{path.name}",
                         pointer.stat().st_size < 1024 and pointer.read_text().startswith("version https://crab.build/spec/v1"))
    runner.run_git(clone, ["fsck", "--full", "--strict"])
    release_verified_run_child(runner, runner.run_root / "cold-clone-cache")
    for snapshot in history:
        version = snapshot["version"]
        # The disposable clone starts dehydrated. Each historical checkout uses
        # a fresh cache so current-version hydration cannot mask lost dependencies.
        runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / f"history-cache-{version}")
        runner.run_git(clone, ["checkout", "--detach", snapshot["commit"]],
                       name=f"v{version} historical checkout")
        measured_hydrate(runner, proxy, read_phases, clone, f"v{version} historical hydrate",
                         logical_bytes, distinct_basis_bytes)
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
        release_verified_run_child(runner, runner.run_root / f"history-cache-{version}")

    # Restore only this invocation's isolated repository, then prove a fresh
    # consumer and a new-epoch publication can still read both file generations.
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
    restored_clone = runner.run_root / "restored-clone"
    runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "restored-clone-cache")
    runner.run_cmd("restored history clone", [runner.crab_bin, "clone", remote, str(restored_clone)], runner.run_root)
    for stage, snapshot in (("restored", oldest), ("republished", history[-1])):
        if stage == "republished":
            runner.run_crab(repo, ["push", "origin", "HEAD:refs/heads/main"],
                            name="publish current version after history restore")
            runner.env["CRAB_CACHE_DIR"] = str(runner.run_root / "republished-clone-cache")
            runner.run_git(restored_clone, ["fetch", "origin"], name="fetch after restore and publication")
            runner.run_git(restored_clone, ["checkout", "--detach", "refs/remotes/origin/main"])
        runner.check(f"{stage}-clone-exact-tip", runner.rev_parse(restored_clone, "HEAD") == snapshot["commit"])
        measured_hydrate(runner, proxy, read_phases, restored_clone, f"{stage} history hydrate",
                         logical_bytes, distinct_basis_bytes)
        for relative, digest in snapshot["files"].items():
            runner.check(f"{stage}-history-bytes-{relative}", sha256_file(restored_clone / relative) == digest)
        runner.run_git(restored_clone, ["fsck", "--full", "--strict"], name=f"{stage} history Git integrity")
        runner.run_crab(restored_clone, ["dehydrate", "--all"], name=f"{stage} history dehydrate")
        cache_name = "restored-clone-cache" if stage == "restored" else "republished-clone-cache"
        release_verified_run_child(runner, runner.run_root / cache_name)
    fsck = measured_read(runner, proxy, read_phases, repo, ["fsck", "--json"],
                         "layered Xet remote fsck")
    fsck_data = json.loads(runner.read_stdout(fsck))["data"]
    runner.check(
        "layered-xet-remote-fsck-clean",
        fsck_data["passed"] and fsck_data["errors"] == 0 and fsck_data["repair_failures"] == 0,
        fsck_data,
    )
    runner.check("binary-unchanged", sha256_file(Path(runner.crab_bin)) == runner.report.artifacts["crab_binary_sha256"])
    runner.check_credential_disclosure()
    runner.report.status = "passed"
    write_transport_report(runner, transport_records, read_phases, proxy.snapshot())
    runner.write_report()
    if args.cleanup:
        # All targets were created by this invocation; retain reports and logs.
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
    args = parser.parse_args()
    if args.files < 1 or args.file_mib < 500 or not 1 <= args.versions <= 11 or args.code_files < 1:
        parser.error("require positive file counts, >=500 MiB/file, and 1–11 versions")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.run_id):
        parser.error("run-id must be a single safe directory name")
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
