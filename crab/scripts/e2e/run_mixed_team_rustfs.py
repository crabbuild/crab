#!/usr/bin/env python3
"""Qualify source-backed teams with alternating native/Git mixed-content pushes.

Reuse the concurrent driver's CLI, receipts and request meter. Supply --source,
--skip-same-branch, --consumer-mode cold-final and at least one update round.
Each developer publishes ordinary Git, Xet and LFS content twice; other rounds
change code. Cold payload readers borrow only independently fetched Git objects.
"""

from __future__ import annotations

import concurrent.futures
import hashlib
import time
from dataclasses import asdict
from pathlib import Path

from run_concurrent_push_smoke import ConcurrentPushSmoke, SmokeError, parse_args
from run_lfs_qualification import hash_file, write_deterministic


MIB = 1024 * 1024


class MixedTeam(ConcurrentPushSmoke):
    def __init__(self, args):
        super().__init__(args)
        self.payload_history: dict[int, list[dict]] = {}
        self.edit_round = max(1, args.branch_update_rounds // 2)
        self.report.artifacts["mixed_team_script_sha256"] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        self.report.workload.update(
            profile="mixed-team", push_client="alternating native/Git batches",
            lfs_profile="crab-native filters and managed pre-push hook",
            xet_bytes_per_developer=MIB, lfs_bytes_per_developer=MIB,
            ordinary_git_blob_bytes_per_developer=64 * 1024,
            payload_edit_round=self.edit_round, partial_edit_bytes=64 * 1024,
            payload_consumer_model="independent data caches borrowing cold Git ODB only",
        )

    def install_helper_alias(self) -> None:
        super().install_helper_alias()
        # Managed LFS hooks invoke `crab` by name; pin them to the tested binary too.
        (self.bin_dir / "crab").symlink_to(Path(self.args.crab_bin).resolve())

    def prepare(self, index: int, round_index: int) -> tuple[str, str, Path, str]:
        branch = f"agent-{index:03d}"
        if round_index == 0:
            repo = self.clone_agent(self.branch_agents, index, "branch-agent")
            self.run_git(repo, ["checkout", "-b", branch])
            self.run_crab(repo, ["lfs", "install", "--local"])
            self.run_crab(repo, ["track", "*.xet"])
            self.run_cmd("track LFS extension", ["git-lfs", "track", "*.lfs"], repo)
        else:
            repo = self.branch_agents / f"branch-agent-{index:03d}"
        directory = repo / "agents" / branch
        directory.mkdir(parents=True, exist_ok=True)
        code = repo / "agents" / f"{branch}.txt"
        with code.open("w" if round_index == 0 else "a") as stream:
            stream.write(f"branch fanout agent {index}\nrun_id {self.run_id}\n"
                         if round_index == 0 else f"update round {round_index}\n")
        files = {}
        if round_index in (0, self.edit_round):
            for name, size in (("ordinary.raw", 64 * 1024), ("model.xet", MIB), ("asset.lfs", MIB)):
                path = directory / name
                if round_index == 0:
                    write_deterministic(path, size, f"{self.run_id}:{index}:{name}", 0)
                else:
                    with path.open("r+b") as stream:
                        stream.seek(size // 2)
                        stream.write(hashlib.shake_256(f"{index}:{name}:edit".encode())
                                     .digest(min(64 * 1024, size // 2)))
                files[str(path.relative_to(repo))] = {"size": path.stat().st_size, "sha256": hash_file(path)}
        self.run_git(repo, ["add", "-A"])
        self.run_git(repo, ["commit", "-m", f"mixed agent {index} round {round_index}"])
        if files:
            revision = self.git_value(repo, ["rev-parse", "HEAD"])
            xet = self.git_value(repo, ["show", f"HEAD:agents/{branch}/model.xet"])
            lfs = self.git_value(repo, ["show", f"HEAD:agents/{branch}/asset.lfs"])
            self.check(f"agent-{index}-round-{round_index}-real-pointer-filters",
                       xet.startswith("version https://crab.build/spec/v1\n")
                       and len(xet) < 512 and lfs.startswith("version https://git-lfs.github.com/spec/v1\n")
                       and f"oid sha256:{files[f'agents/{branch}/asset.lfs']['sha256']}" in lfs)
            with self.report_lock:
                self.payload_history.setdefault(index, []).append({"revision": revision, "files": files})
                self.report.artifacts["payload_history"] = self.payload_history
                self.write_report()
        dst = f"refs/heads/{branch}"
        return f"{branch}-round-{round_index}", dst, repo, f"HEAD:{dst}"

    def prepare_branch_agent(self, index: int) -> tuple[str, str, Path]:
        agent, dst, repo, _ = self.prepare(index, 0)
        return agent, dst, repo

    def run_branch_updates(self) -> None:
        updates = []
        for round_index in range(1, self.args.branch_update_rounds + 1):
            request_before = self.request_proxy.snapshot() if self.request_proxy else None
            started = time.monotonic()
            with concurrent.futures.ThreadPoolExecutor(max_workers=self.args.max_parallel_pushes) as pool:
                jobs = list(pool.map(lambda index: self.prepare(index, round_index), range(self.args.agents)))
            preparation_ms = int((time.monotonic() - started) * 1000)
            # Change client policy between drained batches, never during worker execution.
            self.args.push_client = "git" if round_index % 2 else "native"
            results = self.push_concurrently(jobs)
            self.report.push_batches[-1].update(round=round_index, client=self.args.push_client,
                                                preparation_ms=preparation_ms)
            updates.extend({"round": round_index, "client": self.args.push_client, **asdict(result)}
                           for result in results)
            self.report.branch_updates = updates
            self.check(f"mixed-round-{round_index}-all-acknowledged",
                       len(results) == self.args.agents
                       and all(result.status == "ok" and result.command.exit_code == 0 for result in results))
            self.request_snapshot(f"mixed-round-{round_index}-{self.args.push_client}", request_before,
                                  attempted_pushes=len(results), successful_pushes=len(results))
            self.write_report(force=True)

    def verify_payloads(self, index: int) -> None:
        branch = f"agent-{index:03d}"
        target = self.run_root / "payload-readers" / branch
        cache = self.run_root / "payload-caches" / branch
        self.check(f"{branch}-payload-cache-starts-empty", not cache.exists())
        extra_env = {"CRAB_CACHE_DIR": str(cache)}
        self.run_git(self.run_root, ["clone", "--shared", "--no-checkout", "--single-branch",
                                   "--branch", "main", str(self.run_root / "cold-final"), str(target)])
        self.run_git(target, ["remote", "set-url", "origin", self.remote_url])
        self.run_crab(target, ["init", self.remote_url])
        self.run_crab(target, ["lfs", "install", "--local"])
        self.check(f"{branch}-lfs-cache-starts-empty", not (target / ".git/lfs/objects").exists())
        history = self.payload_history[index]
        self.check(f"{branch}-both-payload-versions-recorded", len(history) == 2)
        for version, entry in enumerate(history):
            self.run_git(target, ["checkout", "--force", "--detach", entry["revision"]], extra_env=extra_env)
            self.run_cmd(f"hydrate {branch} version {version}",
                         [self.args.crab_bin, "hydrate", "--all"], target, extra_env=extra_env)
            self.run_cmd(f"LFS pull {branch} version {version}",
                         [self.args.crab_bin, "lfs", "pull"], target, extra_env=extra_env)
            for name, expected in entry["files"].items():
                path = target / name
                self.check(f"{branch}-version-{version}-{path.name}-exact-bytes",
                           path.is_file() and path.stat().st_size == expected["size"]
                           and hash_file(path) == expected["sha256"])

    def verify_cold_team(self) -> None:
        super().verify_cold_team()
        target = self.run_root / "cold-final"
        baseline = int(self.report.workload["source_commits"])
        expected = baseline + self.args.agents * (1 + self.args.branch_update_rounds)
        actual = int(self.git_value(target, ["rev-list", "--count", "--all"]))
        self.check("all-baseline-and-developer-commits-reachable", actual == expected,
                   {"reachable_commits": actual, "expected": expected})
        (self.run_root / "payload-readers").mkdir()
        with concurrent.futures.ThreadPoolExecutor(max_workers=self.args.max_parallel_readers) as pool:
            futures = [pool.submit(self.verify_payloads, index) for index in range(self.args.agents)]
            try:
                for future in concurrent.futures.as_completed(futures):
                    future.result()
            except BaseException:
                for future in futures:
                    future.cancel()
                raise
        self.check("source-checkout-preserved-after-payload-reads", self.source_state() == self.source_identity)


def main() -> int:
    args = parse_args()
    if (not args.source or args.branch_update_rounds < 1 or not args.skip_same_branch
            or args.skip_branch_fanout or args.consumer_mode != "cold-final"
            or args.push_client != "native" or args.skip_fsck or args.crash_boundary or args.marker_faults):
        raise SmokeError("mixed team requires source, updates, native initial push, cold-final, "
                         "skip-same-branch, fsck, and no crash/fault scenarios")
    return MixedTeam(args).run()


if __name__ == "__main__":
    raise SystemExit(main())
