"""Temporary kernel CPU attribution for the accepted normal binary; no speedup claim."""

from collections import Counter
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import select
import signal
import subprocess
import sys
import threading
import time

from probe_kernel_perf import kernel_perf

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "crab/scripts/e2e"))
from run_existing_ref_push_rustfs import ExistingRefPush
from run_concurrent_push_smoke import SmokeError, parse_args


HEADER = re.compile(r"^\s*(.*?)\s+(\d+)/(\d+)\s+\[(\d+)\]\s+([\d.]+):\s+(\d+)\s+cpu-clock:k:\s*$")
FRAME = re.compile(r"^\s+([0-9a-f]+)\s+(.+?)\s+\((.+)\)\s*$")
PR_SET_CHILD_SUBREAPER = 36


def sha256_file(path):
    with path.open("rb") as data:
        return hashlib.file_digest(data, "sha256").hexdigest()


def decode(script):
    samples, metadata = [], []
    current = None
    for line in script.splitlines():
        match = HEADER.match(line)
        if match:
            comm, pid, tid, cpu, timestamp, period = match.groups()
            current = {"comm": comm, "pid": int(pid), "tid": int(tid), "cpu": int(cpu),
                       "timestamp": timestamp, "period": int(period), "frames": []}
            samples.append(current)
            continue
        match = FRAME.match(line)
        if match and current is not None:
            address, symbol, dso = match.groups()
            current["frames"].append({"ip": address, "symbol": symbol, "dso": dso})
        elif line.strip():
            if "cpu-clock:k:" in line:
                raise SmokeError(f"unparsed sample header: {line}")
            metadata.append(line)
            current = None
        else:
            current = None
    return samples, metadata


class KernelPush(ExistingRefPush):
    def __init__(self, args, perf, profiles):
        super().__init__(args)
        if sys.platform != "linux" or not hasattr(os, "pidfd_open"):
            raise SmokeError("process ownership requires Linux pidfds")
        # Git creates its own process groups. Adopt orphaned descendants so
        # cancelling perf cannot leave a Git writer outside our ownership.
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.prctl(PR_SET_CHILD_SUBREAPER, ctypes.c_ulong(1), ctypes.c_ulong(0), ctypes.c_ulong(0), ctypes.c_ulong(0)) != 0:
            raise OSError(ctypes.get_errno(), "PR_SET_CHILD_SUBREAPER failed")
        self.perf = perf
        self.profiles = profiles
        self.batch = -1
        self.process_lock = threading.RLock()
        self.processes = set()
        self.stopping = False
        self.launching = False
        self.cancelling = False
        self.receipts = []

    def cancel(self):
        self.stopping = True
        # A signal may interrupt Popen before registration. Its caller finishes
        # registration and retries cancellation before communicating with it.
        if self.launching or self.cancelling:
            return
        with self.process_lock:
            self.cancelling = True
            roots = {process.pid for process in self.processes}
            deadline = time.monotonic() + 10
            try:
                while True:
                    pending = False
                    for pid in self.children():
                        try:
                            descriptor = os.pidfd_open(pid)
                        except ProcessLookupError:
                            continue
                        try:
                            # Recheck ownership after opening the stable handle;
                            # a disappearing /proc entry must never redirect a kill.
                            if self.parent(pid) != os.getpid():
                                continue
                            try:
                                signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                            poll = select.poll()
                            poll.register(descriptor, select.POLLIN)
                            if not poll.poll(0):
                                pending = True
                            elif pid not in roots:
                                # Popen alone reaps registered wrappers. Only
                                # adopted orphans are waited here, by exact PID.
                                os.waitpid(pid, os.WNOHANG)
                                pending = True
                        finally:
                            os.close(descriptor)
                    if not pending and self.children() <= roots:
                        return
                    if time.monotonic() >= deadline:
                        raise SmokeError("owned processes did not stop before cleanup deadline")
                    time.sleep(0.01)
            finally:
                self.cancelling = False

    @staticmethod
    def parent(pid):
        try:
            status = Path(f"/proc/{pid}/status").read_text()
        except (FileNotFoundError, ProcessLookupError):
            return None
        return int(re.search(r"^PPid:\s+(\d+)$", status, re.M).group(1))

    @classmethod
    def children(cls):
        return {int(path.name) for path in Path("/proc").iterdir()
                if path.name.isdecimal() and cls.parent(int(path.name)) == os.getpid()}

    def push_concurrently(self, jobs):
        self.batch += 1
        return super().push_concurrently(jobs)

    def run_cmd(self, name, args, cwd, *, check=True, timeout=None, extra_env=None):
        env = self.env.copy()
        if extra_env:
            env.update(extra_env)
        original = args
        profile = None
        actor = Path(cwd).name
        if self.batch > 0 and args[:2] == [self.args.crab_bin, "push"]:
            profile = self.profiles / f"{actor}-round-{self.batch}.data"
            if profile.exists():
                raise SmokeError(f"refusing to overwrite profile: {profile}")
            args = [str(self.perf), "record", "--strict-freq", "-e", "cpu-clock:k", "-F", "999",
                    "-m", "8", "--no-buildid", "--call-graph", "fp", "--kernel-callchains",
                    "--sample-cpu", "-P", "-o", str(profile), "--", *args]
        started = time.monotonic_ns()
        with self.process_lock:
            if self.stopping:
                raise SmokeError("diagnostic cancelled")
            self.launching = True
            try:
                process = subprocess.Popen(args, cwd=cwd, env=env, text=True, start_new_session=True,
                                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                self.processes.add(process)
            finally:
                self.launching = False
        if self.stopping:
            self.cancel()
        failure = None
        try:
            try:
                stdout, stderr = process.communicate(timeout=timeout or self.args.timeout)
            except BaseException as error:
                self.cancel()
                stdout, stderr = process.communicate()
                failure = error
        finally:
            with self.process_lock:
                self.processes.remove(process)
        wall_ms = (time.monotonic_ns() - started) / 1_000_000
        record = self.record_command(name, original, cwd, process.returncode,
                                     int(wall_ms), stdout, stderr)
        if profile:
            receipt = {"actor": actor, "round": self.batch, "record_command": args,
                       "perf_pid": process.pid, "stdout_log": record.stdout_log,
                       "stderr_log": record.stderr_log, "exit_code": process.returncode,
                       "profile": profile.name, "data_sha256": sha256_file(profile) if profile.is_file() else None,
                       "instrumented_wall_ms": wall_ms}
            with self.report_lock:
                self.receipts.append(receipt)
                with (self.profiles / "receipts.jsonl").open("a") as output:
                    output.write(json.dumps(receipt, sort_keys=True) + "\n")
        if failure is not None:
            raise failure
        if check and process.returncode:
            raise SmokeError(f"{name} failed with exit {process.returncode}; stderr log: {record.stderr_log}")
        return record

    def decode_profiles(self):
        updates = self.report.branch_updates
        expected = {(f"branch-agent-{actor:03d}", round_index)
                    for actor in range(50) for round_index in range(1, 9)}
        assert len(updates) == len(self.receipts) == 400
        assert {(row["actor"], row["round"]) for row in self.receipts} == expected
        assert {row["command"]["stdout_log"] for row in updates} == {
            row["stdout_log"] for row in self.receipts}
        leaf_weights, inclusive_weights, comm_weights = Counter(), Counter(), Counter()
        unknown_weight = total_weight = lost_records = empty_profiles = count = 0
        for index, row in enumerate(self.receipts):
            if self.stopping:
                raise SmokeError("diagnostic cancelled")
            data = self.profiles / row["profile"]
            assert row["exit_code"] == 0 and sha256_file(data) == row["data_sha256"]
            script = data.with_suffix(".script")
            command = [str(self.perf), "script", "--ns", "--show-task-events", "--show-lost-events",
                       "--kallsyms", str(self.profiles / "kallsyms.txt"), "-i", str(data),
                       "-F", "comm,pid,tid,cpu,time,event,ip,sym,dso,period"]
            record = self.run_cmd(f"decode {data.stem}", command, self.profiles, timeout=30)
            script.write_bytes(Path(record.stdout_log).read_bytes())
            samples, metadata = decode(script.read_text())
            lost = [line for line in metadata if "PERF_RECORD_LOST" in line]
            lost_records += len(lost)
            empty_profiles += not samples
            count += len(samples)
            for sample in samples:
                period = sample["period"]
                assert period > 0
                total_weight += period
                comm_weights[sample["comm"]] += period
                frames = sample["frames"]
                known = bool(frames) and frames[0]["symbol"] != "[unknown]" and int(frames[0]["ip"], 16) != 0
                leaf = frames[0]["symbol"] if known else "[unknown]"
                unknown_weight += 0 if known else period
                leaf_weights[leaf] += period
                for symbol in {frame["symbol"] for frame in frames}:
                    inclusive_weights[symbol] += period
            payload = {"receipt": row, "render_command": command, "samples": samples,
                       "metadata": metadata, "lost_records": lost}
            data.with_suffix(".json").write_text(json.dumps(payload, indent=2) + "\n")
            if (index + 1) % 50 == 0:
                print(f"Decoded {index + 1}/400 profiles", flush=True)
        summary = {"scope": "Instrumented kernel CPU sample weights; not wall-time decomposition or speedup proof",
                   "updates": len(updates), "profiles": len(self.receipts), "samples": count,
                   "empty_profiles": empty_profiles, "lost_records": lost_records,
                   "total_period_weight": total_weight, "unknown_leaf_weight": unknown_weight,
                   "comm_weights": comm_weights, "leaf_weights": leaf_weights,
                   "inclusive_weights": inclusive_weights}
        (self.profiles / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        self.write_report(force=True)
        if self.stopping or count < 1000 or lost_records:
            raise SmokeError("kernel attribution incomplete: insufficient samples or recorded losses")


def main():
    args = parse_args()
    if (sys.platform != "linux" or args.agents != 50 or args.branch_update_rounds != 8
            or args.max_parallel_pushes != 50 or args.push_client != "native" or not args.source
            or not args.skip_same_branch or args.skip_fsck or args.skip_branch_fanout
            or not args.no_request_capture or args.consumer_mode != "cold-final"):
        raise SmokeError("diagnostic requires Linux, 50 native actors, eight updates and all integrity checks")
    with kernel_perf(Path(os.environ["PERF_BIN"]), Path(os.environ["PROFILE_ROOT"])) as (perf, root, proof):
        proof["scope"] = "Kernel CPU diagnostic for accepted normal Crab release; no performance qualification"
        workload = KernelPush(args, perf, root)

        def cancelled(_number, _frame):
            workload.cancel()

        previous = {number: signal.signal(number, cancelled) for number in (signal.SIGINT, signal.SIGTERM)}
        try:
            if workload.run() != 0:
                raise SmokeError("original real-update/cold-reader/fsck qualification failed")
            workload.decode_profiles()
        finally:
            workload.cancel()
            for number, handler in previous.items():
                signal.signal(number, handler)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
