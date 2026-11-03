#!/usr/bin/env python3
"""Qualify source-backed teams with alternating native/Git mixed-content pushes.

Reuse the concurrent driver's CLI, receipts and request meter. Supply --source,
--skip-same-branch, --consumer-mode cold-final and at least one update round.
Each developer publishes ordinary Git, Xet and LFS content twice; other rounds
change code. Cold payload readers borrow only independently fetched Git objects.
Use --phase push to seal acknowledgements without claiming verified integrity.
Use --phase verify with the original arguments and --run-id to audit that cohort
in a fresh consumer directory on the same filesystem and persistent backend.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import subprocess
import tempfile
import time
from dataclasses import asdict
from pathlib import Path

from run_concurrent_push_smoke import (
    ConcurrentPushSmoke, SmokeError, SmokeReport, parse_args, redact_env, utc_now,
)
from run_lfs_qualification import hash_file, write_deterministic


MIB = 1024 * 1024


class MixedTeam(ConcurrentPushSmoke):
    def __init__(self, args):
        super().__init__(args)
        self.payload_history: dict[int, list[dict]] = {}
        self.next_round = 1
        self.stop_round = getattr(args, "stop_after_round", None)
        if self.stop_round is None:
            self.stop_round = args.branch_update_rounds
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

    def save_batch(self, round_index: int, records: list[dict]) -> None:
        # Preserve failed batches too. A later verification attempt must never
        # replace the original acknowledgements or latency samples.
        self.write_report(force=True)
        self.write_checkpoint(self.artifacts / f"push-batch-{round_index:02d}.json", {
            "schema": "crab.mixed-team-push-batch", "run_id": self.run_id,
            "round": round_index, "batch": self.report.push_batches[-1],
            "pushes": records, "payload_history": self.payload_history,
            "request_snapshots": self.report.request_snapshots[-1:],
            "native_push_http": self.report.artifacts.get("native_push_http", [])[-1:],
        })
        print(json.dumps({"phase": "push-batch", "round": round_index,
                          **self.report.push_batches[-1]}), flush=True)

    def run_branch_fanout(self) -> None:
        try:
            super().run_branch_fanout()
        finally:
            if self.report.branch_fanout:
                self.save_batch(0, self.report.branch_fanout)

    @staticmethod
    def write_checkpoint(path: Path, body: dict) -> None:
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps(body, indent=2, sort_keys=True) + "\n")
        # Publish only a complete file, and reject an attempt to overwrite a
        # retained receipt, including one produced by a failed run.
        try:
            os.link(temporary, path)
        finally:
            temporary.unlink()

    def checkpoint_identity(self) -> dict:
        return {
            "remote_url": self.remote_url, "endpoint_url": self.args.endpoint_url,
            "agents": self.args.agents, "rounds": self.args.branch_update_rounds,
            "push_workers": self.args.max_parallel_pushes,
            "reader_workers": self.args.max_parallel_readers,
            "timeout": self.args.timeout, "push_timeout": self.args.push_timeout,
            "source": str(self.args.source),
            "binary_sha256": hashlib.sha256(Path(self.args.crab_bin).read_bytes()).hexdigest(),
            "git_binary_sha256": hashlib.sha256(Path(self.args.git_bin).read_bytes()).hexdigest(),
            "drivers": {name: hashlib.sha256(Path(__file__).with_name(name).read_bytes()).hexdigest()
                        for name in ("run_mixed_team_rustfs.py", "run_concurrent_push_smoke.py")},
        }

    def seal_pushes(self, through_round: int | None = None) -> None:
        if through_round is None:
            through_round = self.args.branch_update_rounds
        expected = self.args.agents * (1 + through_round)
        records = self.report.branch_fanout + self.report.branch_updates
        if (len(records) != expected or len(self.report.push_batches) != 1 + through_round
                or any(row["status"] != "ok" or row["command"]["exit_code"] != 0 for row in records)):
            raise SmokeError("cannot seal incomplete or rejected pushes")
        branches = sorted(f"refs/heads/agent-{index:03d}" for index in range(self.args.agents))
        for round_index, batch in enumerate(self.report.push_batches):
            cohort = (self.report.branch_fanout if round_index == 0 else
                      [row for row in self.report.branch_updates if row["round"] == round_index])
            if (sorted(row["branch"] for row in cohort) != branches
                    or batch["jobs"] != self.args.agents or batch["successful"] != self.args.agents):
                raise SmokeError("cannot seal a missing or duplicated branch acknowledgement")
        refs = {f"refs/heads/agent-{index:03d}": self.git_value(
                    self.branch_agents / f"branch-agent-{index:03d}", ["rev-parse", "HEAD"])
                for index in range(self.args.agents)}
        refs["refs/heads/main"] = self.git_value(self.seed, ["rev-parse", "HEAD"])
        complete = through_round == self.args.branch_update_rounds
        self.report.status = "pushes-complete" if complete else "pushes-paused"
        self.write_report(force=True)
        report = asdict(self.report)
        name = "push-checkpoint.json" if complete else f"pause-through-{through_round:02d}.json"
        self.write_checkpoint(self.artifacts / name, {
            "schema": "crab.mixed-team-push-checkpoint", "identity": self.checkpoint_identity(),
            "through_round": through_round, "expected_refs": refs, "report": report,
        })

    def restore_report(self, report: dict) -> None:
        self.report = SmokeReport(**report)
        self.command_index = len(self.report.commands)
        self.source_identity = self.report.workload["source_identity"]
        self.payload_history = {int(index): versions for index, versions in
                                self.report.artifacts["payload_history"].items()}

    def resume_pushes(self) -> None:
        report = json.loads((self.artifacts / "report.json").read_text())
        through_round = len(report["push_batches"]) - 1
        checkpoint = self.artifacts / f"pause-through-{through_round:02d}.json"
        saved = json.loads(checkpoint.read_text())
        if (saved.get("schema") != "crab.mixed-team-push-checkpoint"
                or saved.get("identity") != self.checkpoint_identity()
                or saved.get("through_round") != through_round
                or saved["report"] != report or report["status"] != "pushes-paused"
                or not 0 <= through_round < self.stop_round <= self.args.branch_update_rounds):
            raise SmokeError("push resume requires an unchanged, cleanly paused checkpoint")

        # Inspect without the recording helpers: a rejected resume must leave
        # the sealed report and its command logs untouched.
        def git(repo: Path, args: list[str]) -> str:
            result = subprocess.run([self.args.git_bin, "-C", str(repo), *args],
                                    env=self.env, capture_output=True, text=True,
                                    timeout=self.args.timeout, check=True)
            return result.stdout.strip()

        source = {"head": git(self.args.source, ["rev-parse", "HEAD"]),
                  "status": git(self.args.source, ["status", "--porcelain"])}
        if source != report["workload"]["source_identity"]:
            raise SmokeError("push resume source changed")
        for name, oid in saved["expected_refs"].items():
            writer = (self.seed if name == "refs/heads/main" else
                      self.branch_agents / f"branch-{name.removeprefix('refs/heads/')}")
            if (git(writer, ["rev-parse", "HEAD"]) != oid
                    or git(writer, ["status", "--porcelain"])):
                raise SmokeError(f"push resume writer changed: {name}")
        visible = git(self.seed, ["ls-remote", "--refs", self.remote_url])
        refs = {name: oid for oid, name in (line.split() for line in visible.splitlines())}
        if refs != saved["expected_refs"]:
            raise SmokeError("push resume remote refs changed")
        self.restore_report(report)
        self.next_round = through_round + 1
        self.report.status = "running"

    def resume_verification(self) -> None:
        checkpoint = self.artifacts / "push-checkpoint.json"
        raw = checkpoint.read_bytes()
        saved = json.loads(raw)
        if (saved.get("schema") != "crab.mixed-team-push-checkpoint"
                or saved.get("identity") != self.checkpoint_identity()
                or saved["report"]["status"] != "pushes-complete"):
            raise SmokeError("verification checkpoint identity does not match this run")
        self.restore_report(saved["report"])
        # Each attempt gets new logs, Git consumers and data caches. Keep the
        # original writers and the sealed push report as read-only evidence.
        self.run_root = Path(tempfile.mkdtemp(prefix="verification-", dir=self.run_root))
        self.verification_report_started = True
        self.logs = self.run_root / "logs"
        self.artifacts = self.run_root / "artifacts"
        (self.run_root / "tmp").mkdir()
        self.env = self.build_env()
        self.report.root = str(self.run_root)
        self.report.env = redact_env(self.env)
        self.report.status = "verifying"
        self.report.artifacts["push_checkpoint"] = str(checkpoint)
        self.report.artifacts["push_checkpoint_sha256"] = hashlib.sha256(raw).hexdigest()
        self.write_report(force=True)
        self.check("verification-source-unchanged", self.source_state() == self.source_identity)
        for name, oid in saved["expected_refs"].items():
            writer = (self.seed if name == "refs/heads/main" else
                      self.branch_agents / f"branch-{name.removeprefix('refs/heads/')}")
            self.check(f"verification-{name}-writer-unchanged",
                       self.git_value(writer, ["rev-parse", "HEAD"]) == oid)

    def run_branch_updates(self) -> None:
        updates = list(self.report.branch_updates)
        for round_index in range(self.next_round, self.stop_round + 1):
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
            self.request_snapshot(f"mixed-round-{round_index}-{self.args.push_client}", request_before,
                                  attempted_pushes=len(results),
                                  successful_pushes=sum(result.status == "ok" and result.command.exit_code == 0
                                                        for result in results))
            try:
                self.check(f"mixed-round-{round_index}-all-acknowledged",
                           len(results) == self.args.agents
                           and all(result.status == "ok" and result.command.exit_code == 0 for result in results))
            finally:
                self.save_batch(round_index, updates[-len(results):])
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

    def run(self) -> int:
        phase = getattr(self.args, "phase", "all")
        self.verification_report_started = False
        writable = False
        locked = False
        lock = self.args.root / f".{self.run_id}.push-controller"
        try:
            if phase == "verify":
                self.resume_verification()
            else:
                lock.parent.mkdir(parents=True, exist_ok=True)
                lock.mkdir()
                locked = True
                if getattr(self.args, "resume", False):
                    self.resume_pushes()
                    writable = True
                    # Invalidate the paused status before preparing any work.
                    # An interrupted batch cannot reuse the older checkpoint.
                    self.write_report(force=True)
                else:
                    if self.run_root.exists():
                        raise SmokeError(f"refusing to overwrite retained run: {self.run_root}")
                    writable = True
                    self.preflight()
                    self.init_seed()
                    self.run_branch_fanout()
                self.run_branch_updates()
                self.save_meter_total()
                self.seal_pushes(self.stop_round)
            paused = self.report.status == "pushes-paused"
            if phase != "push" and not paused:
                self.verify_cold_team()
                self.run_fsck()
                self.report.status = "ok"
                self.write_report(force=True)
            print(f"{self.report.status}: {self.artifacts / 'report.json'}")
            return 0
        except Exception as error:
            self.report.status = "failed"
            self.report.checks.append({"name": "exception", "ok": False,
                                       "detail": {"error": str(error)}, "timestamp": utc_now()})
            # A rejected resume must not rewrite an existing push report.
            if writable or self.verification_report_started:
                self.write_report(force=True)
            print(f"FAILED: {error}")
            return 1
        finally:
            try:
                if self.request_proxy is not None:
                    try:
                        if self.report.status in ("failed", "ok") and (writable or self.verification_report_started):
                            self.save_meter_total()
                    finally:
                        self.request_proxy.close()
            finally:
                if locked:
                    lock.rmdir()

    def save_meter_total(self) -> None:
        if self.request_proxy is None:
            return
        path = self.artifacts / f"request-meter-total-{time.time_ns()}.json"
        self.write_checkpoint(path, self.request_proxy.snapshot(include_paths=False))
        self.report.artifacts.setdefault("request_meter_totals", []).append(str(path))
        self.report.artifacts["request_meter_total"] = str(path)
        self.write_report(force=True)


def main() -> int:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--phase", choices=["all", "push", "verify"], default="all")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--stop-after-round", type=int)
    phase, remaining = parser.parse_known_args()
    args = parse_args(remaining)
    args.phase = phase.phase
    args.resume = phase.resume
    args.stop_after_round = phase.stop_after_round
    if args.phase == "verify" and not args.run_id:
        raise SmokeError("verification requires the original --run-id")
    if ((args.resume and (not args.run_id or args.phase == "verify"))
            or (args.stop_after_round is not None
                and (args.phase == "verify" or not 0 <= args.stop_after_round <= args.branch_update_rounds))):
        raise SmokeError("push resumption requires --run-id; stop round must be within the planned push rounds")
    if (not args.source or args.branch_update_rounds < 1 or not args.skip_same_branch
            or args.skip_branch_fanout or args.consumer_mode != "cold-final"
            or args.push_client != "native" or args.skip_fsck or args.crash_boundary or args.marker_faults):
        raise SmokeError("mixed team requires source, updates, native initial push, cold-final, "
                         "skip-same-branch, fsck, and no crash/fault scenarios")
    return MixedTeam(args).run()


if __name__ == "__main__":
    raise SystemExit(main())
