"""Temporary Linux resource diagnostic; never a performance qualification."""

import inspect
import json
import os
from pathlib import Path
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "crab/scripts/e2e"))
from run_existing_ref_push_rustfs import ExistingRefPush
from run_concurrent_push_smoke import SmokeError, parse_args


class MeasuredPopen(subprocess.Popen):
    usage = None

    def _try_wait(self, wait_flags):
        # CPython's POSIX wait holds _waitpid_lock here. Replace only the
        # reaping syscall; communicate still drains both pipes and owns timeouts.
        try:
            pid, status, usage = os.wait4(self.pid, wait_flags)
        except ChildProcessError:
            return self.pid, 0
        if pid == self.pid:
            self.usage = usage
        return pid, status


class ResourcePush(ExistingRefPush):
    batch = -1

    def push_concurrently(self, jobs):
        self.batch += 1
        return super().push_concurrently(jobs)

    def run_cmd(self, name, args, cwd, *, check=True, timeout=None, extra_env=None):
        env = self.env.copy()
        if extra_env:
            env.update(extra_env)
        original = args
        trace = None
        actor = Path(cwd).name
        if args[0] == self.args.crab_bin and "push" in args and self.batch > 0:
            stem = Path(os.environ["RESOURCE_ROOT"]) / f"{actor}-round-{self.batch}"
            if actor == "branch-agent-000":
                trace = "syscall-summary"
                args = ["strace", "-f", "-c", "-o", str(stem) + ".syscalls", *args]
            elif actor == "branch-agent-001" and self.batch in {1, 3}:
                trace = "file-events"
                args = ["strace", "-f", "-qq", "-e",
                        "trace=openat,newfstatat,statx,readlink,clone,clone3",
                        "-o", str(stem) + ".files", *args]
        start = time.monotonic_ns()
        with MeasuredPopen(args, cwd=cwd, env=env, text=True,
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE) as proc:
            try:
                stdout, stderr = proc.communicate(timeout=timeout or self.args.timeout)
            except BaseException:
                proc.kill()
                proc.communicate()
                raise
        wall_ms = (time.monotonic_ns() - start) / 1_000_000
        if proc.usage is None:
            raise SmokeError("wait4 resource receipt missing; refusing incomplete attribution")
        record = self.record_command(name, original, cwd, proc.returncode,
                                     int(wall_ms), stdout, stderr)
        fields = ("utime", "stime", "maxrss", "minflt", "majflt", "inblock",
                  "oublock", "nvcsw", "nivcsw")
        row = {"stdout_log": record.stdout_log, "name": name, "cwd": str(cwd),
               "batch": self.batch, "trace": trace, "wall_ms": wall_ms,
               "exit_code": proc.returncode,
               **{key: getattr(proc.usage, "ru_" + key) for key in fields}}
        with self.report_lock:
            with (Path(os.environ["RESOURCE_ROOT"]) / "resources.jsonl").open("a") as output:
                output.write(json.dumps(row, sort_keys=True) + "\n")
        if check and proc.returncode:
            raise SmokeError(f"{name} failed with exit {proc.returncode}; stderr log: {record.stderr_log}")
        return record


def main():
    root = Path(os.environ["RESOURCE_ROOT"])
    root.mkdir(parents=True, exist_ok=True)
    runtime = {"python": sys.version,
               "try_wait": inspect.getsource(subprocess.Popen._try_wait),
               "wait": inspect.getsource(subprocess.Popen._wait),
               "communicate": inspect.getsource(subprocess.Popen.communicate)}
    (root / "python-runtime.json").write_text(json.dumps(runtime, indent=2) + "\n")
    args = parse_args()
    if (sys.platform != "linux" or args.agents != 50 or args.branch_update_rounds != 8
            or args.push_client != "native" or not args.source or not args.skip_same_branch
            or args.skip_fsck or args.skip_branch_fanout or not args.no_request_capture):
        raise SmokeError("diagnostic requires Linux, pinned source, 50 native actors, eight updates and integrity checks")
    return ResourcePush(args).run()


if __name__ == "__main__":
    raise SystemExit(main())
